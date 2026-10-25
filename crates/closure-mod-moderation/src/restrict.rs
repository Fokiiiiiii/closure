//! Times members out and mutes them in voice, each either by Discord's own feature or by a role.
//!
//! A role is an ordinary role: the server decides what it denies, in the channels and categories,
//! and the bot's top role must be above it. A timer takes the role off when the time is up. The
//! timer names the role it was given, so a later change of the settings does not strand it.

use std::time::Duration;

use closure_sdk::{
    Args, Case, CommandArgs, CommandCx, Cx, DiscordError, InteractionRef, JobCx, ModuleError,
    Reason, RoleId, ScheduleError, UserError, UserId,
};
use serde::{Deserialize, Serialize};

use crate::templates;
use crate::{
    Config, DAY, MemberReason, audit_reason, dm_reason, mention, notify, reason_of, record,
    revoke_latest, secs, timeout_seconds,
};

/// How a timeout or a voice mute is given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Method {
    /// Discord's own feature.
    #[default]
    Discord,
    /// A role, which a timer takes off again.
    Role,
}

/// How long a voice unmute that Discord refuses waits before it is tried again.
const POSTPONE: Duration = Duration::from_secs(600);

/// The payload of an `untimeout` job: the member, the case of the timeout, and its role.
#[derive(Debug, Serialize, Deserialize)]
struct RoleTimeout {
    user: u64,
    case: i64,
    role: u64,
}

/// The payload of a `vc_unmute` job. `due` is when the mute was due: a lift that Discord keeps
/// refusing is given up a day after it.
#[derive(Debug, Serialize, Deserialize)]
struct VoiceLift {
    user: u64,
    case: i64,
    via: Method,
    role: Option<u64>,
    due: i64,
}

/// `/vcmute`: a member and, optionally, how long.
#[derive(CommandArgs)]
pub(crate) struct VoiceMute {
    /// Member to mute in voice channels
    user: UserId,
    /// How long, such as 1h or 7d; until lifted when left out
    duration: Option<Duration>,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

fn untimeout_key(user: UserId) -> String {
    format!("untimeout:{}", user.get())
}

fn voice_key(user: UserId) -> String {
    format!("vcunmute:{}", user.get())
}

/// The role a setting names, when it is a valid role id.
fn role_of(setting: Option<&str>) -> Option<RoleId> {
    setting?.parse::<u64>().ok().and_then(RoleId::new_checked)
}

/// The role a timeout gives. A missing or unusable one is refused before anything is done.
pub(crate) fn timeout_role(config: &Config) -> Result<RoleId, UserError> {
    role_of(config.timeout_role.as_deref())
        .ok_or_else(|| UserError::message("timeout-role-missing"))
}

/// The role a voice mute gives.
fn voice_role(config: &Config) -> Result<RoleId, UserError> {
    role_of(config.voice_mute_role.as_deref())
        .ok_or_else(|| UserError::message("voice-role-missing"))
}

/// Refuses a timeout that the configured method cannot give, before anything is done. Discord's
/// own timeout lasts 28 days at most; a role has no such limit. A timeout whose duration is not
/// known yet is checked for its role only.
pub(crate) fn check_timeout(config: &Config, duration: Option<Duration>) -> Result<(), UserError> {
    match config.timeout_method {
        Method::Discord => duration.map_or(Ok(()), |duration| timeout_seconds(duration).map(drop)),
        Method::Role => timeout_role(config).map(drop),
    }
}

/// Gives `user` a timeout for `duration`. Returns the role when a timer has to take it off.
pub(crate) async fn give_timeout<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    config: &Config,
    user: UserId,
    duration: Duration,
    reason: Option<&str>,
) -> Result<Option<RoleId>, UserError> {
    match config.timeout_method {
        Method::Discord => {
            let seconds = timeout_seconds(duration)?;
            cx.api()
                .timeout(cx.guild_id(), user, Some(seconds), audit_reason(cx, reason))
                .await?;
            Ok(None)
        }
        Method::Role => {
            let role = timeout_role(config)?;
            cx.api()
                .add_role(cx.guild_id(), user, role, audit_reason(cx, reason))
                .await?;
            Ok(Some(role))
        }
    }
}

/// Sets the timer that takes the timeout role off `user` after `after`. A timer set earlier for
/// the member is replaced, since it would take the role off too early.
pub(crate) async fn schedule_timeout_lift<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    user: UserId,
    case: i64,
    role: RoleId,
    after: Duration,
) -> Result<(), ScheduleError> {
    let _ = cx.scheduler().cancel(&untimeout_key(user)).await;
    let lift = RoleTimeout {
        user: user.get(),
        case,
        role: role.get(),
    };
    cx.scheduler()
        .job("untimeout", &lift)?
        .key(&untimeout_key(user))
        .after(after)
        .await
}

/// Lifts the timeout of `user`, by Discord's or by the timeout role. The role's timer is
/// cancelled, best effort; Discord's timeout has no timer of its own.
pub(crate) async fn lift_timeout(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    reason: Option<&str>,
) -> Result<(), UserError> {
    match config.timeout_method {
        Method::Discord => {
            cx.api()
                .timeout(cx.guild_id(), user, None, audit_reason(cx, reason))
                .await?;
        }
        Method::Role => {
            let role = timeout_role(config)?;
            cx.api()
                .remove_role(cx.guild_id(), user, role, audit_reason(cx, reason))
                .await?;
            let _ = cx.scheduler().cancel(&untimeout_key(user)).await;
        }
    }
    Ok(())
}

/// Takes the timeout role off when its time is up. A member or role that is gone is done already.
pub(crate) async fn end_timeout(cx: JobCx) -> Result<(), ModuleError> {
    let payload: RoleTimeout = cx.payload()?;
    let (Some(user), Some(role)) = (
        UserId::new_checked(payload.user),
        RoleId::new_checked(payload.role),
    ) else {
        return Err(ModuleError::other("a timeout job names no member or role"));
    };
    let reason = Reason::truncated(&format!("the timeout of case #{} ran out", payload.case));
    match cx
        .api()
        .remove_role(cx.guild_id(), user, role, reason)
        .await
    {
        Ok(()) | Err(DiscordError::NotFound) => Ok(()),
        Err(error) => Err(ModuleError::Discord(error)),
    }
}

/// Mutes `user` in voice, by Discord's or by the voice role. Returns the role a timer takes off.
async fn mute_in_voice<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    config: &Config,
    user: UserId,
    reason: Option<&str>,
) -> Result<Option<RoleId>, UserError> {
    match config.voice_mute_method {
        Method::Discord => {
            cx.api()
                .set_voice_mute(cx.guild_id(), user, true, audit_reason(cx, reason))
                .await?;
            Ok(None)
        }
        Method::Role => {
            let role = voice_role(config)?;
            cx.api()
                .add_role(cx.guild_id(), user, role, audit_reason(cx, reason))
                .await?;
            Ok(Some(role))
        }
    }
}

/// Lifts the voice mute of `user`, by Discord's or by the voice role, and cancels its timer. The
/// timer is best effort: the member is unmuted already.
async fn unmute_in_voice(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    reason: Option<&str>,
) -> Result<(), UserError> {
    match config.voice_mute_method {
        Method::Discord => {
            cx.api()
                .set_voice_mute(cx.guild_id(), user, false, audit_reason(cx, reason))
                .await?;
        }
        Method::Role => {
            let role = voice_role(config)?;
            cx.api()
                .remove_role(cx.guild_id(), user, role, audit_reason(cx, reason))
                .await?;
        }
    }
    let _ = cx.scheduler().cancel(&voice_key(user)).await;
    Ok(())
}

/// Sets the timer that lifts the voice mute of `user` (case `case`) after `duration`. The caller
/// cancels an earlier timer of the member first.
async fn schedule_voice_lift<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    config: &Config,
    user: UserId,
    case: i64,
    role: Option<RoleId>,
    duration: Duration,
) -> Result<(), ScheduleError> {
    let lift = VoiceLift {
        user: user.get(),
        case,
        via: config.voice_mute_method,
        role: role.map(RoleId::get),
        due: cx.now() + secs(duration),
    };
    cx.scheduler()
        .job("vc_unmute", &lift)?
        .key(&voice_key(user))
        .after(duration)
        .await
}

/// Mutes `user` in voice and records it. With a `duration` a timer lifts the mute when it is up.
pub(crate) async fn mute<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    config: &Config,
    user: UserId,
    duration: Option<Duration>,
    reason: Option<String>,
) -> Result<(Case, Vec<String>), UserError> {
    let now = cx.now();
    let reason_text = dm_reason(cx, reason.as_deref());
    let role = mute_in_voice(cx, config, user, reason.as_deref()).await?;
    let expires_at = duration.map(|duration| now + secs(duration));
    let case = record(cx, "vcmute", user, reason, expires_at).await?;
    // A new mute replaces the timer of an earlier one, which would lift this mute too early.
    let _ = cx.scheduler().cancel(&voice_key(user)).await;
    let timer_note = match duration {
        Some(duration) => schedule_voice_lift(cx, config, user, case.number, role, duration)
            .await
            .is_err()
            .then(|| cx.t("vc-no-timer", &Args::new())),
        None => None,
    };
    let note = match duration {
        Some(duration) => {
            notify(
                cx,
                config,
                user,
                "dm-voice-muted-for",
                &Args::new()
                    .with("duration", cx.format_duration_in(cx.ctx().locale, duration))
                    .with("reason", reason_text),
            )
            .await
        }
        None => {
            notify(
                cx,
                config,
                user,
                "dm-voice-muted",
                &Args::new().with("reason", reason_text),
            )
            .await
        }
    };
    Ok((case, [note, timer_note].into_iter().flatten().collect()))
}

/// `/vcmute`: mutes a member in voice, for `duration` or until `/vcunmute`. Without a reason, a
/// template is asked for.
pub(crate) async fn voice_mute(cx: CommandCx, args: VoiceMute) -> Result<(), UserError> {
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let punishment = templates::Punishment::new(
        templates::Kind::Vcmute,
        args.user,
        reason_of(args.reason),
        args.duration,
        None,
    );
    templates::punish(&cx, &config, punishment).await
}

/// `/vcunmute`: lifts the voice mute of a member and revokes the newest mute that stands.
pub(crate) async fn voice_unmute(cx: CommandCx, args: MemberReason) -> Result<(), UserError> {
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let reason = reason_of(args.reason);
    unmute_in_voice(&cx, &config, args.user, reason.as_deref()).await?;
    let case = record(&cx, "vcunmute", args.user, reason, None).await?;
    revoke_latest(
        &cx,
        args.user,
        "vcmute",
        format!("removed by case #{}", case.number),
    )
    .await;
    cx.reply_private_t(
        "voice-unmuted",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
    )
    .await
}

/// Lifts a voice mute when its time is up. Discord refuses it while the member is not in voice:
/// the lift is then tried again in ten minutes, but given up a day after the mute was due.
pub(crate) async fn end_voice_mute(cx: JobCx) -> Result<(), ModuleError> {
    let payload: VoiceLift = cx.payload()?;
    let Some(user) = UserId::new_checked(payload.user) else {
        return Err(ModuleError::other("a voice mute job names no member"));
    };
    let reason = Reason::truncated(&format!("the voice mute of case #{} ran out", payload.case));
    let result = match (payload.via, payload.role.and_then(RoleId::new_checked)) {
        (Method::Discord, _) => {
            cx.api()
                .set_voice_mute(cx.guild_id(), user, false, reason)
                .await
        }
        (Method::Role, Some(role)) => {
            cx.api()
                .remove_role(cx.guild_id(), user, role, reason)
                .await
        }
        (Method::Role, None) => {
            return Err(ModuleError::other("a voice mute job names no role"));
        }
    };
    match result {
        Err(DiscordError::NotInVoice) if cx.now() - payload.due <= DAY => cx
            .scheduler()
            .job("vc_unmute", &payload)
            .map_err(|error| ModuleError::other(error.to_string()))?
            .key(&voice_key(user))
            .after(POSTPONE)
            .await
            .map_err(|error| ModuleError::other(error.to_string())),
        // Done: nothing is left to lift, or the lift has been refused for a day.
        Ok(()) | Err(DiscordError::NotFound | DiscordError::NotInVoice) => Ok(()),
        Err(error) => Err(ModuleError::Discord(error)),
    }
}
