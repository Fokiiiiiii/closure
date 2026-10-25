//! Gives back the punishments a member still has when the member rejoins. Leaving the server
//! takes a role off, and a timeout is lost the same way, so a rejoin would end a punishment early.
//! Only what still stands is given again: a case that was revoked or ran out is not.

use closure_sdk::{
    Args, Case, DiscordError, Event, EventContext, Flow, ModuleError, Reason, UserId,
};

use crate::restrict::{self, Method};
use crate::{Config, MAX_TIMEOUT_SECONDS, active_cases};

/// What runs when a member joins: the newest timeout and voice mute that stand are given again.
pub(crate) async fn on_join(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MemberJoined(member) = event else {
        return Ok(Flow::Continue);
    };
    if member.user.bot {
        return Ok(Flow::Continue);
    }
    let config: Config = closure_sdk::parse_module_config(&ctx)?;
    let user = member.user.id;
    // Both are tried even when the first fails, so one refused action does not undo the other.
    let timeout = match standing(&ctx, user, "timeout").await? {
        Some(case) => give_timeout_again(&ctx, &config, user, &case).await,
        None => Ok(()),
    };
    let voice = match standing(&ctx, user, "vcmute").await? {
        Some(case) => give_voice_mute_again(&ctx, &config, user, &case).await,
        None => Ok(()),
    };
    timeout.and(voice)?;
    Ok(Flow::Continue)
}

/// The newest case of `kind` that still stands against `user`.
async fn standing(
    ctx: &EventContext,
    user: UserId,
    kind: &str,
) -> Result<Option<Case>, ModuleError> {
    let cases = active_cases(ctx.cases(), user, kind, ctx.now(), 1)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(cases.into_iter().next())
}

/// Gives a timeout again. A role is given as it was; Discord's timeout is given for the time that
/// is left, at most its own limit.
async fn give_timeout_again(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    case: &Case,
) -> Result<(), DiscordError> {
    let reason = rejoin_reason(ctx, "rejoin-timeout", case.number);
    let result = match config.timeout_method {
        Method::Role => {
            // A missing or unusable role gives nothing; the setting is the server's to fix.
            let Ok(role) = restrict::timeout_role(config) else {
                return Ok(());
            };
            ctx.api.add_role(ctx.guild_id, user, role, reason).await
        }
        Method::Discord => {
            // A timeout is always recorded with its expiry, so one without it is not given.
            let Some(expires_at) = case.expires_at else {
                return Ok(());
            };
            let left = expires_at - ctx.now();
            if left <= 0 {
                return Ok(());
            }
            let seconds = u32::try_from(left.min(i64::from(MAX_TIMEOUT_SECONDS)))
                .unwrap_or(MAX_TIMEOUT_SECONDS);
            ctx.api
                .timeout(ctx.guild_id, user, Some(seconds), reason)
                .await
        }
    };
    tolerate_gone(result)
}

/// Gives a voice mute again by its role. Discord's server mute is not given: Discord refuses it
/// while the member is not in a voice channel, which is where a rejoining member is not.
async fn give_voice_mute_again(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    case: &Case,
) -> Result<(), DiscordError> {
    match config.voice_mute_method {
        Method::Discord => Ok(()),
        Method::Role => {
            let Ok(role) = restrict::voice_role(config) else {
                return Ok(());
            };
            let reason = rejoin_reason(ctx, "rejoin-voice", case.number);
            tolerate_gone(ctx.api.add_role(ctx.guild_id, user, role, reason).await)
        }
    }
}

/// A member who is gone again is done already, so a `NotFound` is no failure.
fn tolerate_gone(result: Result<(), DiscordError>) -> Result<(), DiscordError> {
    match result {
        Err(DiscordError::NotFound) => Ok(()),
        other => other,
    }
}

/// The audit reason, in the language of the server.
fn rejoin_reason(ctx: &EventContext, key: &str, number: i64) -> Reason {
    Reason::truncated(&ctx.t_in(ctx.locale, key, &Args::new().with("number", number)))
}
