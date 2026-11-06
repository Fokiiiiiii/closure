//! `POST /v1/guilds/{id}/messages`: the dashboard writes a message, and the bot sends it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use closure_core::event::{ChannelId, GuildId, MessageId};
use closure_core::message::MessageSpec;
use closure_core::module::ModuleId;
use closure_core::schedule::{NewJob, RunAt, ScheduleError, SchedulerBackend};
use serde::Deserialize;
use serde_json::{Value, json};
use twilight_model::channel::message::{Component, Embed};

use crate::{
    ApiState,
    access::evaluate,
    config::{actor, guild_id, stored},
    error::ApiError,
};

/// A message that was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    pub id: u64,
}

/// Why Discord did not take a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendFailure {
    /// The bot may not write in that channel.
    Forbidden,
    NotFound,
    Unavailable,
    /// Discord refused the message itself.
    Rejected(String),
}

/// What a message holds when it is posted.
#[derive(Debug, Clone, Default)]
pub struct Post {
    pub content: Option<String>,
    pub embeds: Vec<Embed>,
    pub components: Vec<Component>,
}

/// Where the API sends the messages it was asked to: Discord, in the running bot.
pub trait MessageSink: Send + Sync {
    /// Sends `post` to `channel`, or, with `edit`, replaces that message of the bot with it.
    fn send(
        &self,
        channel: ChannelId,
        edit: Option<MessageId>,
        post: Post,
    ) -> Pin<Box<dyn Future<Output = Result<Sent, SendFailure>> + Send + '_>>;
}

/// The most posts that wait for their time in one server.
const MAX_SCHEDULED: usize = 50;
/// How far ahead a post can be scheduled.
const MAX_AHEAD_SECONDS: i64 = 366 * 24 * 3600;
/// How far back a time may be and still count as now.
const GRACE_SECONDS: i64 = 60;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendBody {
    channel_id: String,
    /// A [`MessageSpec`]; kept as it was written so that a scheduled post is stored whole.
    message: Value,
    /// The id of a message of the bot to replace instead of sending a new one.
    #[serde(default)]
    edit: Option<String>,
    /// Unix seconds: send then, not now.
    #[serde(default)]
    send_at: Option<i64>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

fn snowflake(text: &str, what: &str) -> Result<u64, ApiError> {
    text.parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && i64::try_from(*id).is_ok())
        .ok_or_else(|| ApiError::BadRequest(format!("{what} must be an id")))
}

/// Checks that the caller may write to the server's dashboard; returns the server.
async fn authorize(state: &ApiState, raw: u64, headers: &HeaderMap) -> Result<GuildId, ApiError> {
    let guild = guild_id(raw)?;
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }
    if let Some(user) = actor(headers)? {
        let (_, current) = stored(state, guild).await?;
        let settings = state.inner.settings.compiler().compile(&current)?;
        if !evaluate(state, guild, user, settings.permissions())
            .await?
            .allowed
        {
            return Err(ApiError::Forbidden(
                "sending messages needs the permission to edit the dashboard".into(),
            ));
        }
    }
    Ok(guild)
}

pub(crate) async fn send(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
    headers: HeaderMap,
    Json(body): Json<SendBody>,
) -> Result<Json<Value>, ApiError> {
    let sink: Arc<dyn MessageSink> = state.messages.clone().ok_or(ApiError::Unavailable)?;
    let status = state.inner.readiness.gateway();
    if status.total == 0 || status.ready != status.total {
        return Err(ApiError::Unavailable);
    }
    let guild = authorize(&state, raw, &headers).await?;
    let channel = snowflake(&body.channel_id, "channel_id")?;
    let edit = body
        .edit
        .as_deref()
        .map(|id| snowflake(id, "edit"))
        .transpose()?;
    let known = state
        .inner
        .directory
        .directory(guild)
        .ok_or(ApiError::Unavailable)?
        .channels
        .iter()
        .any(|c| c.id == body.channel_id && matches!(c.kind, "text" | "announcement"));
    if !known {
        return Err(ApiError::NotFound(
            "that text channel is not in the server".into(),
        ));
    }
    let spec: MessageSpec = serde_json::from_value(body.message.clone())
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let (content, embeds, components) = spec
        .into_post(now(), &str::to_owned)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    if let Some(at) = body.send_at {
        return schedule(
            &state,
            guild,
            at,
            json!({
                "channel": channel.to_string(),
                "edit": edit.map(|id| id.to_string()),
                "message": body.message,
            }),
        )
        .await;
    }
    let channel_id = ChannelId::new(channel);
    let sent = sink
        .send(
            channel_id,
            edit.map(MessageId::new),
            Post {
                content,
                embeds,
                components,
            },
        )
        .await
        .map_err(|failure| match failure {
            SendFailure::Forbidden => {
                ApiError::Forbidden("the bot cannot write in that channel".into())
            }
            SendFailure::NotFound => {
                ApiError::NotFound("that channel or message no longer exists".into())
            }
            SendFailure::Unavailable => ApiError::Unavailable,
            SendFailure::Rejected(why) => ApiError::BadRequest(why),
        })?;
    tracing::info!(
        guild = raw,
        channel,
        message = sent.id,
        edited = edit.is_some(),
        "a message was sent from the dashboard"
    );
    Ok(Json(json!({
        "id": sent.id.to_string(),
        "channel_id": channel.to_string(),
        "url": format!("https://discord.com/channels/{raw}/{channel}/{}", sent.id),
    })))
}

/// Keeps `payload` for the `post` module to send at `at`.
async fn schedule(
    state: &ApiState,
    guild: GuildId,
    at: i64,
    payload: Value,
) -> Result<Json<Value>, ApiError> {
    let current = now();
    if at < current - GRACE_SECONDS || at > current + MAX_AHEAD_SECONDS {
        return Err(ApiError::BadRequest(
            "send_at must be between now and a year ahead".into(),
        ));
    }
    if state.inner.db.scheduled_posts(guild).await?.len() >= MAX_SCHEDULED {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_SCHEDULED} posts can wait at once"
        )));
    }
    let key = format!(
        "p{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let job = NewJob {
        module: ModuleId::new("post").map_err(|e| ApiError::Internal(e.to_string()))?,
        guild,
        name: "send".into(),
        payload,
        run_at: RunAt::At(at.max(current)),
        every: None,
        key: Some(key.clone()),
    };
    state
        .inner
        .db
        .schedule(job, false)
        .await
        .map_err(|error| match error {
            ScheduleError::TooLarge { .. } | ScheduleError::Payload(_) => {
                ApiError::BadRequest("that message is too large to be scheduled".into())
            }
            other => ApiError::Internal(other.to_string()),
        })?;
    tracing::info!(
        guild = guild.get(),
        at,
        "a post was scheduled from the dashboard"
    );
    Ok(Json(
        json!({ "scheduled": true, "key": key, "send_at": at }),
    ))
}

/// `GET /v1/guilds/{id}/scheduled-posts`: the posts waiting for their time.
pub(crate) async fn scheduled(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let guild = authorize(&state, raw, &headers).await?;
    let posts: Vec<Value> = state
        .inner
        .db
        .scheduled_posts(guild)
        .await?
        .into_iter()
        .map(|post| {
            json!({
                "key": post.key,
                "send_at": post.run_at,
                "channel_id": post.payload["channel"],
                "edit": post.payload["edit"],
                "message": post.payload["message"],
            })
        })
        .collect();
    Ok(Json(json!({ "posts": posts })))
}

/// `DELETE /v1/guilds/{id}/scheduled-posts/{key}`: the post will not be sent.
pub(crate) async fn unschedule(
    State(state): State<ApiState>,
    Path((raw, key)): Path<(u64, String)>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let guild = authorize(&state, raw, &headers).await?;
    let module = ModuleId::new("post").map_err(|e| ApiError::Internal(e.to_string()))?;
    let removed = state
        .inner
        .db
        .cancel(&module, guild, &key)
        .await
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound("no such scheduled post".into()))
    }
}
