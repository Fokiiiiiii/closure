//! `POST /v1/guilds/{id}/messages`: the dashboard writes a message, and the bot sends it.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use closure_core::event::ChannelId;
use closure_core::message::MessageSpec;
use serde::Deserialize;
use serde_json::{Value, json};
use twilight_model::channel::message::Embed;

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

/// Where the API sends the messages it was asked to: Discord, in the running bot.
pub trait MessageSink: Send + Sync {
    fn send(
        &self,
        channel: ChannelId,
        content: Option<String>,
        embeds: Vec<Embed>,
    ) -> Pin<Box<dyn Future<Output = Result<Sent, SendFailure>> + Send + '_>>;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendBody {
    channel_id: String,
    message: MessageSpec,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

pub(crate) async fn send(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
    headers: HeaderMap,
    Json(body): Json<SendBody>,
) -> Result<Json<Value>, ApiError> {
    let guild = guild_id(raw)?;
    let sink: Arc<dyn MessageSink> = state.messages.clone().ok_or(ApiError::Unavailable)?;
    let status = state.inner.readiness.gateway();
    if status.total == 0 || status.ready != status.total {
        return Err(ApiError::Unavailable);
    }
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }
    if let Some(user) = actor(&headers)? {
        let (_, current) = stored(&state, guild).await?;
        let settings = state.inner.settings.compiler().compile(&current)?;
        if !evaluate(&state, guild, user, settings.permissions())
            .await?
            .allowed
        {
            return Err(ApiError::Forbidden(
                "sending messages needs the permission to edit the dashboard".into(),
            ));
        }
    }
    let channel = body
        .channel_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && i64::try_from(*id).is_ok())
        .ok_or_else(|| ApiError::BadRequest("channel_id must be a channel id".into()))?;
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
    let (content, embeds) = body
        .message
        .into_parts(now(), &str::to_owned)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let channel_id = ChannelId::new(channel);
    let sent = sink
        .send(channel_id, content, embeds)
        .await
        .map_err(|failure| match failure {
            SendFailure::Forbidden => {
                ApiError::Forbidden("the bot cannot write in that channel".into())
            }
            SendFailure::NotFound => ApiError::NotFound("that channel no longer exists".into()),
            SendFailure::Unavailable => ApiError::Unavailable,
            SendFailure::Rejected(why) => ApiError::BadRequest(why),
        })?;
    tracing::info!(
        guild = raw,
        channel,
        message = sent.id,
        "a message was sent from the dashboard"
    );
    Ok(Json(json!({
        "id": sent.id.to_string(),
        "channel_id": channel.to_string(),
        "url": format!("https://discord.com/channels/{raw}/{channel}/{}", sent.id),
    })))
}
