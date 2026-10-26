//! The restrictions of members as commands: `/vcmute` and `/vcunmute`, and the checks of the role
//! and duration settings that timeouts and voice mutes need. The punishment itself, and its timer,
//! is carried out by `closure_sdk::punish`.
//!
//! A role is an ordinary role: the server decides what it denies, in the channels and categories,
//! and the bot's top role must be above it.

use std::time::Duration;

use closure_sdk::punish::Method;
use closure_sdk::{Args, CommandArgs, CommandCx, RoleId, Span, Tone, UserError, UserId};

use crate::templates;
use crate::{Config, MemberReason, mention, punisher_for, reason_of, timeout_seconds};

/// `/vcmute`: a member and, optionally, how long.
#[derive(CommandArgs)]
pub(crate) struct VoiceMute {
    /// Member to mute in voice channels
    user: UserId,
    /// How long, such as 1h or 7d, or until a date and time; until lifted when left out
    duration: Option<Span>,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

/// The role a setting names, when it is a valid role id.
pub(crate) fn role_of(setting: Option<&str>) -> Option<RoleId> {
    setting?.parse::<u64>().ok().and_then(RoleId::new_checked)
}

/// Refuses a timeout that Discord's own timeout cannot give: it lasts 28 days at most. A role has
/// no such limit, and its role is found or created when the punishment runs.
pub(crate) fn check_timeout(config: &Config, duration: Option<Duration>) -> Result<(), UserError> {
    match config.timeout_method {
        Method::Discord => duration.map_or(Ok(()), |duration| timeout_seconds(duration).map(drop)),
        Method::Role => Ok(()),
    }
}

/// `/vcmute`: mutes a member in voice, for `duration` or until `/vcunmute`. Without a reason, a
/// template is asked for.
pub(crate) async fn voice_mute(cx: CommandCx, args: VoiceMute) -> Result<(), UserError> {
    if let Some(span) = &args.duration {
        span.resolve(&cx)?;
    }
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
    let case = punisher_for(&cx, &config)
        .await?
        .voice_unmute(args.user, reason_of(args.reason))
        .await?;
    cx.say_private_t(
        Tone::Success,
        "voice-unmuted",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
    )
    .await
}
