//! Which invite a member joined with: the uses of the invites are kept, and the one whose count
//! went up since the last look is the one the member came by.
//!
//! Discord does not say it. The guess is right when one member joins between two looks; for
//! several at once, or an invite that ran out or was deleted by then, the entry says so instead of
//! naming one.

use std::collections::BTreeMap;
use std::time::Duration;

use closure_core::error::ModuleError;
use closure_core::module::EventContext;

const USES_KEY: &str = "invite-uses";
/// How often the uses are looked at without a join, so that the count a join is compared with is
/// not old.
pub(crate) const SNAPSHOT_EVERY: Duration = Duration::from_secs(600);
/// The least time between two lookups for one server: a raid of joins does not make a request for
/// each, and the Discord rate limit is not spent on it.
const LOOKUP_GAP: i64 = 2;
/// The most invites told apart in one entry.
const LISTED: usize = 3;

type Uses = BTreeMap<String, u64>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    Invite {
        code: String,
        inviter: Option<u64>,
    },
    /// Several invites counted a use since the last look: more than one member joined.
    Several(Vec<String>),
    /// No invite counted it: a vanity address, an invite that is gone, an integration, or no earlier
    /// look to compare with.
    Unknown,
}

const LOOKED_KEY: &str = "invite-looked-at";

/// Whether this server may be looked up now; if so, the time is taken. The joins of one server are
/// handled one after the other, so the check and the taking do not race.
async fn may_look(ctx: &EventContext) -> bool {
    let now = ctx.now();
    let last: Option<i64> = ctx.store().get(LOOKED_KEY).await.ok().flatten();
    if last.is_some_and(|at| now - at < LOOKUP_GAP) {
        return false;
    }
    ctx.store().set(LOOKED_KEY, &now).await.is_ok()
}

async fn read(ctx: &EventContext) -> Result<Vec<(String, u64, Option<u64>)>, ModuleError> {
    let invites = ctx.api.list_invites(ctx.guild_id).await?;
    Ok(invites
        .into_iter()
        .map(|invite| {
            (
                invite.code,
                invite.uses.unwrap_or(0),
                invite.inviter.map(|user| user.id.get()),
            )
        })
        .collect())
}

pub(crate) async fn snapshot(ctx: &EventContext) -> Result<(), ModuleError> {
    let uses: Uses = read(ctx)
        .await?
        .into_iter()
        .map(|(code, uses, _)| (code, uses))
        .collect();
    ctx.store()
        .set(USES_KEY, &uses)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))
}

/// The invite a member joined with, or `None` when it cannot be looked up now (the bot may not
/// manage the server, or another join was looked up a moment ago).
pub(crate) async fn joined_with(ctx: &EventContext) -> Option<Found> {
    if !may_look(ctx).await {
        return None;
    }
    let listed = match read(ctx).await {
        Ok(listed) => listed,
        Err(error) => {
            tracing::debug!(%error, "could not look at the invites");
            return None;
        }
    };
    let before: Option<Uses> = ctx.store().get(USES_KEY).await.ok().flatten();
    let now: Uses = listed
        .iter()
        .map(|(code, uses, _)| (code.clone(), *uses))
        .collect();
    if let Err(error) = ctx.store().set(USES_KEY, &now).await {
        tracing::warn!(%error, "could not keep the uses of the invites");
    }
    // With no earlier look there is nothing to compare with.
    let Some(before) = before else {
        return Some(Found::Unknown);
    };
    let mut used: Vec<&(String, u64, Option<u64>)> = listed
        .iter()
        .filter(|(code, uses, _)| *uses > before.get(code).copied().unwrap_or(0))
        .collect();
    used.sort_by(|a, b| a.0.cmp(&b.0));
    Some(match used.as_slice() {
        [] => Found::Unknown,
        [(code, _, inviter)] => Found::Invite {
            code: code.clone(),
            inviter: *inviter,
        },
        many => Found::Several(
            many.iter()
                .take(LISTED)
                .map(|(code, ..)| code.clone())
                .collect(),
        ),
    })
}
