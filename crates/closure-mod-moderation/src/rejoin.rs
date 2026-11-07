//! Gives back the punishments a member still has when the member rejoins. Leaving the server
//! takes a role off, and a timeout is lost the same way, so a rejoin would end a punishment early.
//! Only what still stands is given again: a case that was revoked or ran out is not.

use std::time::Duration;

use closure_sdk::punish::{
    Method, Moderator, Punisher, RestrictionRole, RoleError, ensure_role, member_turn,
};
use closure_sdk::{
    Args, Case, DiscordError, Event, EventContext, Flow, ModuleError, Reason, RoleId, UserId,
};

use crate::restrict;
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
    let _turn = member_turn(&ctx, user).await;
    // Both are tried even when the first fails, so one refused action does not undo the other.
    let timeout = match standing(&ctx, user, "timeout").await? {
        Some(case) => give_timeout_again(&ctx, &config, user, &case).await,
        None => Ok(()),
    };
    let voice = match standing(&ctx, user, "vcmute").await? {
        Some(case) => give_voice_mute_again(&ctx, &config, user, &case).await,
        None => Ok(()),
    };
    // The staff are told whatever happened above: a refused role does not hide the history.
    if let Err(error) = alert_staff(&ctx, &config, &member.user).await {
        tracing::debug!(%error, guild = %ctx.guild_id, "the staff alert on a rejoin was not posted");
    }
    timeout.and(voice)?;
    Ok(Flow::Continue)
}

/// How many cases of the member the alert reads and how many it lists.
const ALERT_READ: usize = 25;
const ALERT_LISTED: usize = 5;

/// Tells the staff channel that a member with a history is back: how many cases they have, and the
/// newest ones. Nothing is posted for a member with no case, or when the alert is off.
async fn alert_staff(
    ctx: &EventContext,
    config: &Config,
    user: &closure_sdk::UserInfo,
) -> Result<(), ModuleError> {
    let Some(channel) = config.alert_target().filter(|_| config.alert_on_rejoin) else {
        return Ok(());
    };
    let cases = ctx
        .cases()
        .for_target(user.id, None, ALERT_READ)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let cases: Vec<_> = cases.into_iter().filter(|case| !case.hidden).collect();
    if cases.is_empty() {
        return Ok(());
    }
    let say = |key: &str, args: &Args| ctx.t_in(ctx.locale, key, args);
    let more = if cases.len() >= ALERT_READ { "+" } else { "" };
    let mut text = say(
        "rejoin-alert",
        &Args::new()
            .with("user", format!("<@{}>", user.id))
            .with("count", format!("{}{more}", cases.len())),
    );
    for case in cases.iter().take(ALERT_LISTED) {
        let reason = case.reason.as_deref().map_or_else(
            || say("rejoin-alert-no-reason", &Args::new()),
            crate::shorten,
        );
        text.push('\n');
        text.push_str(&say(
            "rejoin-alert-case",
            &Args::new()
                .with("number", case.number)
                .with("kind", case.kind.as_str().to_owned())
                .with("moderator", format!("<@{}>", case.moderator))
                .with("reason", reason),
        ));
    }
    ctx.api
        .execute(closure_core::action::Action::SendMessage {
            channel_id: channel,
            content: Some(text),
            embeds: Vec::new(),
            reply_to: None,
            mentions: closure_core::action::MentionPolicy::None,
        })
        .await?;
    Ok(())
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
            let role = match ensure_role(
                ctx,
                RestrictionRole::Timeout,
                restrict::role_of(config.timeout_role.as_deref()),
            )
            .await
            {
                Ok(role) => role,
                Err(RoleError::Discord(error)) => return Err(error),
                // The role is set up again by the next punishment: nothing is given now.
                Err(_) => return Ok(()),
            };
            match ctx.api.add_role(ctx.guild_id, user, role, reason).await {
                Ok(()) => rearm(ctx, config, user, case, role, false).await,
                Err(error) => Err(error),
            }
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

/// Schedules the lift of a role given again on rejoin, for the time the case has left. The timer that
/// lifted the role before may have lifted another one, so it is replaced.
async fn rearm(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    case: &Case,
    role: RoleId,
    voice: bool,
) -> Result<(), DiscordError> {
    let Some(expires_at) = case.expires_at else {
        return Ok(());
    };
    let left = u64::try_from(expires_at.saturating_sub(ctx.now())).unwrap_or(0);
    let punisher = Punisher::new(
        ctx,
        Moderator::bot(ctx, "rejoin").map_err(|error| DiscordError::Fatal(error.to_string()))?,
        crate::settings_from(config, (!voice).then_some(role), voice.then_some(role)),
    );
    let after = Duration::from_secs(left);
    let scheduled = if voice {
        punisher
            .rearm_voice_lift(user, case.number, role, after)
            .await
    } else {
        punisher
            .rearm_timeout_lift(user, case.number, role, after)
            .await
    };
    scheduled.map_err(|error| DiscordError::Fatal(format!("could not schedule the lift: {error}")))
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
            let role = match ensure_role(
                ctx,
                RestrictionRole::Voice,
                restrict::role_of(config.voice_mute_role.as_deref()),
            )
            .await
            {
                Ok(role) => role,
                Err(RoleError::Discord(error)) => return Err(error),
                Err(_) => return Ok(()),
            };
            let reason = rejoin_reason(ctx, "rejoin-voice", case.number);
            tolerate_gone(
                match ctx.api.add_role(ctx.guild_id, user, role, reason).await {
                    Ok(()) => rearm(ctx, config, user, case, role, true).await,
                    Err(error) => Err(error),
                },
            )
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
