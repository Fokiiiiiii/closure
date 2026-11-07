use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use closure_core::action::{Action, ChannelPatch, Outcome, OverwriteSpec, OverwriteTarget};
use closure_sdk::duration::{TimeStyle, timestamp};
use closure_sdk::{
    Args, ChannelId, CommandArgs, CommandCx, Event, EventContext, Flow, GuildId, JobCx,
    ModuleError, Reason, Span, Tone, UserError,
};
use closure_sdk::{embed, modlog};
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
pub(crate) struct LongSlowmode {
    /// Seconds between two messages of a member: 21601 to 2592000, or 0 to turn it off
    #[arg(min = 0, max = 2_592_000)]
    seconds: i64,
    /// Channel, or the current channel
    channel: Option<ChannelId>,
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
async fn display_name(cx: &CommandCx) -> String {
    cx.api()
        .get_member(cx.guild_id(), cx.user().id)
        .await
        .ok()
        .map_or_else(
            || cx.user().name.clone(),
            |member| {
                member
                    .nick
                    .or(member.user.global_name)
                    .unwrap_or(member.user.name)
            },
        )
        .replace(['\n', '\r'], " ")
}

fn audit_reason(cx: &CommandCx, name: &str, reason: Option<&str>) -> Reason {
    let user = cx.user();
    let text = cx.ctx().t_args(
        "channel-audit-reason",
        &Args::new()
            .with("id", user.id.get().to_string())
            .with("name", name.to_owned())
            .with("reason", reason.unwrap_or("")),
    );
    Reason::truncated(&text)
}

async fn log(ctx: &EventContext, actor: &str, id: ChannelId, action: &str, until: Option<i64>) {
    let expiry = until.map_or_else(String::new, |time| {
        ctx.t_args(
            "channel-log-until",
            &Args::new().with("until", timestamp(time, TimeStyle::LongDateTime)),
        )
    });
    let message = ctx.t_args(
        "channel-log",
        &Args::new()
            .with("actor", actor.to_owned())
            .with("channel", id.get().to_string())
            .with("action", action.to_owned())
            .with("expiry", expiry),
    );
    let _ = modlog::post_text(ctx, message).await;
}

fn actor(cx: &CommandCx, name: &str) -> String {
    format!("{} ({})", embed::escape(name), cx.user().id)
}

async fn edit(
    ctx: &EventContext,
    channel: ChannelId,
    seconds: u16,
    reason: Reason,
) -> Result<(), ModuleError> {
    ctx.api
        .execute(Action::EditChannel {
            channel_id: channel,
            patch: ChannelPatch {
                slowmode_seconds: Some(seconds),
                ..ChannelPatch::default()
            },
            reason,
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
    let name = display_name(&cx).await;
    let gate = gate(cx.guild_id());
    let _guard = gate.lock().await;
    let info = channel(cx.ctx(), id).await?;
    let key = key("slowmode", id);
    let until = duration
        .map(|duration| {
            i64::try_from(duration.as_secs())
                .map(|seconds| cx.now() + seconds)
                .map_err(failure)
        })
        .transpose()?;
    if let Some(until) = until {
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
        edit(cx.ctx(), id, seconds, audit_reason(&cx, &name, None)).await?;
        cx.scheduler().cancel(&key).await?;
        cx.store().delete(&key).await?;
    }
    if duration.is_some() {
        edit(cx.ctx(), id, seconds, audit_reason(&cx, &name, None)).await?;
    }
    log(
        cx.ctx(),
        &actor(&cx, &name),
        id,
        &cx.ctx().t_args(
            "channel-log-slowmode",
            &Args::new().with("seconds", i64::from(seconds)),
        ),
        until,
    )
    .await;
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
    let name = display_name(&cx).await;
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
            reason: audit_reason(&cx, &name, args.reason.as_deref()),
        })
        .await?;
    log(
        cx.ctx(),
        &actor(&cx, &name),
        id,
        &cx.ctx().t("channel-log-lock"),
        until,
    )
    .await;
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
    reason: Reason,
) -> Result<(), ModuleError> {
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
    let name = display_name(&cx).await;
    let gate = gate(cx.guild_id());
    let _guard = gate.lock().await;
    let key = key("lock", id);
    let Some(saved) = cx.store().get::<SavedLock>(&key).await? else {
        return Err(UserError::message("lock-not-found"));
    };
    restore_overwrite(cx.ctx(), id, saved, audit_reason(&cx, &name, None)).await?;
    cx.scheduler().cancel(&key).await?;
    log(
        cx.ctx(),
        &actor(&cx, &name),
        id,
        &cx.ctx().t("channel-log-unlock"),
        None,
    )
    .await;
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
        edit(
            cx.ctx(),
            id,
            saved.original,
            Reason::truncated(&cx.ctx().t("channel-action-reason")),
        )
        .await?;
        cx.store().delete(&key).await.map_err(failure)?;
        log(
            cx.ctx(),
            &cx.ctx().t("channel-log-system"),
            id,
            &cx.ctx().t_args(
                "channel-log-slowmode",
                &Args::new().with("seconds", i64::from(saved.original)),
            ),
            None,
        )
        .await;
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
        restore_overwrite(
            cx.ctx(),
            id,
            saved,
            Reason::truncated(&cx.ctx().t("channel-action-reason")),
        )
        .await?;
        log(
            cx.ctx(),
            &cx.ctx().t("channel-log-system"),
            id,
            &cx.ctx().t("channel-log-unlock"),
            None,
        )
        .await;
    }
    Ok(())
}

/// The channels whose slowmode is longer than Discord allows, with its seconds.
type LongChannels = HashMap<u64, i64>;
const LONG_KEY: &str = "longslow";
/// The longest slowmode Discord itself keeps; the bot keeps the rest.
const DISCORD_MAX: u16 = 21600;

/// What each server has set, kept in memory so a message costs no read.
#[derive(Default)]
pub(crate) struct LongState {
    servers: Mutex<HashMap<GuildId, Arc<LongChannels>>>,
}

impl LongState {
    fn held(&self, guild: GuildId) -> Option<Arc<LongChannels>> {
        self.servers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&guild)
            .cloned()
    }

    fn hold(&self, guild: GuildId, channels: Arc<LongChannels>) {
        self.servers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(guild, channels);
    }

    async fn channels(&self, ctx: &EventContext) -> Result<Arc<LongChannels>, ModuleError> {
        if let Some(held) = self.held(ctx.guild_id) {
            return Ok(held);
        }
        let loaded: LongChannels = ctx
            .store()
            .get(LONG_KEY)
            .await
            .map_err(failure)?
            .unwrap_or_default();
        let loaded = Arc::new(loaded);
        self.hold(ctx.guild_id, Arc::clone(&loaded));
        Ok(loaded)
    }
}

/// Sets a slowmode longer than Discord's: Discord keeps its longest, and the bot removes a message
/// that comes too soon after the member's last one.
pub(crate) async fn long_slowmode(
    state: Arc<LongState>,
    cx: CommandCx,
    args: LongSlowmode,
) -> Result<(), UserError> {
    if (1..=i64::from(DISCORD_MAX)).contains(&args.seconds) {
        return Err(UserError::message("longslow-range"));
    }
    let id = args.channel.unwrap_or(cx.invocation().channel_id);
    cx.defer(true).await?;
    let name = display_name(&cx).await;
    let gate = gate(cx.guild_id());
    let _guard = gate.lock().await;
    let mut channels = (*state.channels(cx.ctx()).await?).clone();
    let was_kept = if args.seconds == 0 {
        channels.remove(&id.get()).is_some()
    } else {
        channels.insert(id.get(), args.seconds);
        true
    };
    cx.store().set(LONG_KEY, &channels).await?;
    state.hold(cx.guild_id(), Arc::new(channels));
    if was_kept {
        let native = if args.seconds == 0 { 0 } else { DISCORD_MAX };
        edit(cx.ctx(), id, native, audit_reason(&cx, &name, None)).await?;
    }
    log(
        cx.ctx(),
        &actor(&cx, &name),
        id,
        &cx.ctx().t_args(
            "channel-log-slowmode",
            &Args::new().with("seconds", args.seconds),
        ),
        None,
    )
    .await;
    cx.say_t(
        Tone::Success,
        "longslow-done",
        &Args::new()
            .with("channel", id.get().to_string())
            .with("seconds", args.seconds),
    )
    .await
}

/// A message in a channel with a long slowmode: removed when the member's last one is too near.
pub(crate) async fn enforce_long_slowmode(
    state: Arc<LongState>,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = &event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot {
        return Ok(Flow::Continue);
    }
    let channels = state.channels(&ctx).await?;
    let Some(&seconds) = channels.get(&message.channel_id.get()) else {
        return Ok(Flow::Continue);
    };
    let config: crate::Config = ctx.parse_config()?;
    if message.author_roles.iter().any(|role| {
        config
            .slowmode_exempt_roles
            .contains(&role.get().to_string())
    }) {
        return Ok(Flow::Continue);
    }
    let name = format!("longslow-{}", message.channel_id.get());
    let now = ctx.now();
    let last = ctx
        .store()
        .score(&name, message.author.id)
        .await
        .map_err(failure)?;
    if last > 0 && now - last < seconds {
        let reason = Reason::truncated(&ctx.t_args("longslow-reason", &Args::new()));
        if let Err(error) = ctx
            .api
            .delete_message(message.channel_id, message.id, reason)
            .await
        {
            tracing::debug!(?error, guild = %ctx.guild_id, "could not remove a message sent too soon");
        }
    } else {
        ctx.store()
            .score_put(&name, message.author.id, now)
            .await
            .map_err(failure)?;
    }
    Ok(Flow::Continue)
}
