use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use closure_core::action::{Action, ChannelPatch, Outcome, OverwriteSpec, OverwriteTarget};
use closure_sdk::{
    Args, ChannelId, CommandArgs, CommandCx, EventContext, GuildId, JobCx, ModuleError, Reason,
    Span, Tone, UserError,
};
use serde::{Deserialize, Serialize};
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

#[derive(CommandArgs)]
pub(crate) struct Slowmode {
    /// Slowmode seconds, from 0 to 21600
    #[arg(min = 0, max = 21600)]
    seconds: i64,
    /// Channel, or the current channel
    channel: Option<ChannelId>,
    /// How long before restoring the previous value
    duration: Option<Span>,
}

#[derive(CommandArgs)]
pub(crate) struct Lock {
    /// Channel, or the current channel
    channel: Option<ChannelId>,
    /// How long before restoring the previous overwrite
    duration: Option<Span>,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
pub(crate) struct Unlock {
    /// Channel, or the current channel
    channel: Option<ChannelId>,
}

#[derive(Serialize, Deserialize)]
struct SavedSlowmode {
    original: u16,
    until: i64,
}
#[derive(Serialize, Deserialize)]
struct SavedLock {
    original: Option<OverwriteSpec>,
    until: Option<i64>,
}

fn gate(guild: GuildId) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<GuildId, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        gates
            .entry(guild)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}
fn key(kind: &str, channel: ChannelId) -> String {
    format!("channel:{kind}:{}", channel.get())
}
fn failure(error: impl std::fmt::Display) -> ModuleError {
    ModuleError::other(error.to_string())
}

async fn channel(
    ctx: &EventContext,
    id: ChannelId,
) -> Result<twilight_model::channel::Channel, ModuleError> {
    match ctx
        .api
        .execute(Action::GetChannel { channel_id: id })
        .await?
    {
        Outcome::Channel(channel) => Ok(*channel),
        _ => Err(failure("unexpected outcome for get_channel")),
    }
}
async fn schedule(
    ctx: &EventContext,
    job: &str,
    key: &str,
    channel: ChannelId,
    until: i64,
) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(job, &channel)
        .map_err(failure)?
        .key(key)
        .at(until)
        .await
        .map_err(failure)
}
async fn edit(ctx: &EventContext, channel: ChannelId, seconds: u16) -> Result<(), ModuleError> {
    ctx.api
        .execute(Action::EditChannel {
            channel_id: channel,
            patch: ChannelPatch {
                slowmode_seconds: Some(seconds),
                ..ChannelPatch::default()
            },
            reason: Reason::truncated(&ctx.t("channel-action-reason")),
        })
        .await?;
    Ok(())
}

pub(crate) async fn slowmode(cx: CommandCx, args: Slowmode) -> Result<(), UserError> {
    let seconds = u16::try_from(args.seconds)
        .ok()
        .filter(|n| *n <= 21600)
        .ok_or_else(|| UserError::message("slowmode-range"))?;
    let duration = args.duration.map(|span| span.resolve(&cx)).transpose()?;
    let id = args.channel.unwrap_or(cx.invocation().channel_id);
    cx.defer(true).await?;
    let gate = gate(cx.guild_id());
    let _guard = gate.lock().await;
    let info = channel(cx.ctx(), id).await?;
    let key = key("slowmode", id);
    if let Some(duration) = duration {
        let until = cx.now() + i64::try_from(duration.as_secs()).map_err(failure)?;
        let saved = cx.store().get::<SavedSlowmode>(&key).await?;
        let original = match saved {
            Some(saved) => saved.original,
            None => info
                .rate_limit_per_user
                .ok_or_else(|| failure("channel slowmode is unavailable"))?,
        };
        schedule(cx.ctx(), "channel_restore_slowmode", &key, id, until).await?;
        cx.store()
            .set(&key, &SavedSlowmode { original, until })
            .await?;
    } else {
        edit(cx.ctx(), id, seconds).await?;
        cx.scheduler().cancel(&key).await?;
        cx.store().delete(&key).await?;
    }
    if duration.is_some() {
        edit(cx.ctx(), id, seconds).await?;
    }
    cx.say_t(
        Tone::Success,
        "slowmode-done",
        &Args::new()
            .with("channel", id.get().to_string())
            .with("seconds", i64::from(seconds)),
    )
    .await
}

pub(crate) async fn lock(cx: CommandCx, args: Lock) -> Result<(), UserError> {
    let duration = args.duration.map(|span| span.resolve(&cx)).transpose()?;
    let id = args.channel.unwrap_or(cx.invocation().channel_id);
    cx.defer(true).await?;
    let gate = gate(cx.guild_id());
    let _guard = gate.lock().await;
    let key = key("lock", id);
    let info = channel(cx.ctx(), id).await?;
    let target = OverwriteTarget::Role(Id::new(cx.guild_id().get()));
    let existing = info
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| failure("channel overwrites are unavailable"))?
        .iter()
        .find(|item| {
            item.kind == PermissionOverwriteType::Role && item.id.get() == cx.guild_id().get()
        })
        .map(|item| OverwriteSpec {
            target,
            allow: item.allow,
            deny: item.deny,
        });
    let saved = cx.store().get::<SavedLock>(&key).await?;
    let original = saved.map_or_else(|| existing.clone(), |saved| saved.original);
    let until = duration
        .map(|duration| {
            i64::try_from(duration.as_secs())
                .map(|seconds| cx.now() + seconds)
                .map_err(failure)
        })
        .transpose()?;
    if let Some(until) = until {
        schedule(cx.ctx(), "channel_restore_lock", &key, id, until).await?;
    } else {
        cx.scheduler().cancel(&key).await?;
    }
    cx.store().set(&key, &SavedLock { original, until }).await?;
    let bits = Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::ADD_REACTIONS;
    let mut overwrite = existing.unwrap_or(OverwriteSpec {
        target,
        allow: Permissions::empty(),
        deny: Permissions::empty(),
    });
    overwrite.allow.remove(bits);
    overwrite.deny.insert(bits);
    cx.api()
        .execute(Action::SetPermissionOverwrite {
            channel_id: id,
            overwrite,
            reason: Reason::truncated(&cx.t("channel-action-reason", &Args::new())),
        })
        .await?;
    let reason = args
        .reason
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| cx.t("reason-none", &Args::new()), |s| crate::shorten(&s));
    cx.say_t(
        Tone::Success,
        "lock-done",
        &Args::new()
            .with("channel", id.get().to_string())
            .with("reason", reason),
    )
    .await
}

async fn restore_overwrite(
    ctx: &EventContext,
    id: ChannelId,
    saved: SavedLock,
) -> Result<(), ModuleError> {
    let reason = Reason::truncated(&ctx.t("channel-action-reason"));
    let action = match saved.original {
        Some(overwrite) => Action::SetPermissionOverwrite {
            channel_id: id,
            overwrite,
            reason,
        },
        None => Action::DeletePermissionOverwrite {
            channel_id: id,
            target: OverwriteTarget::Role(Id::new(ctx.guild_id.get())),
            reason,
        },
    };
    ctx.api.execute(action).await?;
    ctx.store()
        .delete(&key("lock", id))
        .await
        .map_err(failure)?;
    Ok(())
}

pub(crate) async fn unlock(cx: CommandCx, args: Unlock) -> Result<(), UserError> {
    let id = args.channel.unwrap_or(cx.invocation().channel_id);
    cx.defer(true).await?;
    let gate = gate(cx.guild_id());
    let _guard = gate.lock().await;
    let key = key("lock", id);
    let Some(saved) = cx.store().get::<SavedLock>(&key).await? else {
        return Err(UserError::message("lock-not-found"));
    };
    restore_overwrite(cx.ctx(), id, saved).await?;
    cx.scheduler().cancel(&key).await?;
    cx.say_t(
        Tone::Success,
        "unlock-done",
        &Args::new().with("channel", id.get().to_string()),
    )
    .await
}

pub(crate) async fn restore_slowmode(cx: JobCx) -> Result<(), ModuleError> {
    let id: ChannelId = cx.payload()?;
    let gate = gate(cx.guild_id());
    let _guard = gate.lock().await;
    let key = key("slowmode", id);
    if let Some(saved) = cx
        .store()
        .get::<SavedSlowmode>(&key)
        .await
        .map_err(failure)?
    {
        if saved.until > cx.now() {
            return schedule(cx.ctx(), "channel_restore_slowmode", &key, id, saved.until).await;
        }
        edit(cx.ctx(), id, saved.original).await?;
        cx.store().delete(&key).await.map_err(failure)?;
    }
    Ok(())
}

pub(crate) async fn restore_lock(cx: JobCx) -> Result<(), ModuleError> {
    let id: ChannelId = cx.payload()?;
    let gate = gate(cx.guild_id());
    let _guard = gate.lock().await;
    let key = key("lock", id);
    if let Some(saved) = cx.store().get::<SavedLock>(&key).await.map_err(failure)?
        && let Some(until) = saved.until
    {
        if until > cx.now() {
            return schedule(cx.ctx(), "channel_restore_lock", &key, id, until).await;
        }
        restore_overwrite(cx.ctx(), id, saved).await?;
    }
    Ok(())
}
