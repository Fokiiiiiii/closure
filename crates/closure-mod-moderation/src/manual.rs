//! Punishments made by hand in Discord are recorded as cases, with the moderator the audit log names.
//! A ban, a kick and a timeout are recorded; an unban or a lifted timeout ends the case that stands.
//! What the bot did itself is already a case and is not recorded twice.

use closure_core::action::{Action, AuditLogQuery, Outcome};
use closure_core::cases::{CaseKind, CaseSource, NewCase};
use closure_sdk::duration::snowflake_time;
use closure_sdk::{Event, EventContext, Flow, ModuleError, UserId};
use twilight_model::guild::audit_log::{AuditLogEntry, AuditLogEventType};

use crate::{Config, active_cases};

/// How old an audit log entry may be and still be the change an event reports, in seconds.
const WINDOW: i64 = 60;
const LOOKED_AT: u16 = 50;
/// A case of the same kind recorded this recently (in seconds) is the one the event reports.
const RECENT_CASE: i64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Ban,
    Unban,
    Kick,
    Timeout(i64),
    Untimeout,
}

impl Change {
    fn kind(self) -> &'static str {
        match self {
            Self::Ban | Self::Unban => "ban",
            Self::Kick => "kick",
            Self::Timeout(_) | Self::Untimeout => "timeout",
        }
    }

    fn audit(self) -> AuditLogEventType {
        match self {
            Self::Ban => AuditLogEventType::MemberBanAdd,
            Self::Unban => AuditLogEventType::MemberBanRemove,
            Self::Kick => AuditLogEventType::MemberKick,
            Self::Timeout(_) | Self::Untimeout => AuditLogEventType::MemberUpdate,
        }
    }

    /// Whether the change ends a punishment instead of making one.
    fn lifts(self) -> bool {
        matches!(self, Self::Unban | Self::Untimeout)
    }
}

fn change_of(event: &Event, now: i64) -> Option<(Change, UserId)> {
    match event {
        Event::BanAdded { user, .. } => Some((Change::Ban, user.id)),
        Event::BanRemoved { user, .. } => Some((Change::Unban, user.id)),
        // Discord reports a kick as a leave; the audit log tells which one it was.
        Event::MemberLeft { user, .. } => Some((Change::Kick, user.id)),
        Event::TimeoutChanged { user, after, .. } => match *after {
            Some(until) if until > now => Some((Change::Timeout(until), user.id)),
            _ => Some((Change::Untimeout, user.id)),
        },
        _ => None,
    }
}

/// The newest entry of the audit log that is the change, with who made it and why.
async fn entry_of(ctx: &EventContext, change: Change, target: UserId) -> Option<AuditLogEntry> {
    let query = AuditLogQuery {
        user_id: None,
        action_type: Some(u16::from(change.audit())),
        before: None,
        limit: Some(LOOKED_AT),
    };
    let outcome = ctx
        .api
        .execute(Action::GetAuditLog {
            guild_id: ctx.guild_id,
            query,
        })
        .await;
    let Ok(Outcome::AuditLog(log)) = outcome else {
        tracing::debug!(guild = %ctx.guild_id, "the audit log is not read: a manual punishment is not recorded");
        return None;
    };
    let now = ctx.now();
    log.entries.into_iter().find(|entry| {
        entry.target_id.is_some_and(|id| id.get() == target.get())
            && now - snowflake_time(entry.id.get()) <= WINDOW
    })
}

/// What runs for a ban, an unban, a kick (a leave) and a change of a timeout.
pub(crate) async fn on_change(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let config: Config = closure_sdk::parse_module_config(&ctx)?;
    if !config.manual_cases {
        return Ok(Flow::Continue);
    }
    let Some((change, target)) = change_of(&event, ctx.now()) else {
        return Ok(Flow::Continue);
    };
    if event_is_about_a_bot(&event) {
        return Ok(Flow::Continue);
    }
    let Some(entry) = entry_of(&ctx, change, target).await else {
        return Ok(Flow::Continue);
    };
    let Some(by) = entry.user_id.filter(|by| by.get() != target.get()) else {
        // A member who changes their own timeout, or an entry that names no one.
        return Ok(Flow::Continue);
    };
    // What the bot did is a case already.
    if ctx.bot() == Some(by) {
        return Ok(Flow::Continue);
    }
    let reason = entry.reason.clone().map(|text| truncated(&text));
    let cases = ctx.cases();
    if change.lifts() {
        lift(&ctx, change, target, by, reason).await?;
        return Ok(Flow::Continue);
    }
    // Not twice: a case of this kind made a moment ago by the module is this one.
    let recent = cases
        .for_target(target, None, 5)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let now = ctx.now();
    if recent
        .iter()
        .any(|case| case.kind.as_str() == change.kind() && now - case.created_at <= RECENT_CASE)
    {
        return Ok(Flow::Continue);
    }
    let expires_at = match change {
        Change::Timeout(until) => Some(until),
        _ => None,
    };
    cases
        .create(NewCase {
            source: CaseSource::Manual,
            kind: CaseKind::new(change.kind())
                .map_err(|error| ModuleError::other(error.to_string()))?,
            target,
            moderator: by,
            reason,
            expires_at,
        })
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(Flow::Continue)
}

/// Ends the cases of the kind that stand against `target`: they were lifted by hand.
async fn lift(
    ctx: &EventContext,
    change: Change,
    target: UserId,
    by: UserId,
    reason: Option<String>,
) -> Result<(), ModuleError> {
    let standing = active_cases(ctx.cases(), target, change.kind(), ctx.now(), 10)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    for case in standing {
        // Revoked once: a lift that raced with another is not an error.
        let _ = ctx.cases().revoke(case.number, by, reason.clone()).await;
    }
    Ok(())
}

fn event_is_about_a_bot(event: &Event) -> bool {
    match event {
        Event::BanAdded { user, .. }
        | Event::BanRemoved { user, .. }
        | Event::MemberLeft { user, .. }
        | Event::TimeoutChanged { user, .. } => user.bot,
        _ => false,
    }
}

/// A reason as a case keeps it: at most 512 characters.
fn truncated(text: &str) -> String {
    closure_core::action::Reason::truncated(text)
        .as_str()
        .to_owned()
}
