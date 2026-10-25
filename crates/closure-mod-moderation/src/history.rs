//! The history line of a punishment: what the member had before it, as the last line of the reply.
//! It is counted before the action runs, so the case of the action is not in it.

use closure_sdk::{Args, CaseError, Cases, Cx, InteractionRef, UserId};

use crate::{PAGE, SCAN, active_cases, count};

/// What a member had before a punishment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Before {
    /// The warnings that still count.
    warnings: usize,
    /// The cases of any kind, at most [`SCAN`].
    cases: usize,
    /// Whether there are more cases than `cases` counts.
    more: bool,
}

/// What the member had before the action, or nothing when the cases cannot be read. The line is
/// then left out, and the action goes on as it would have.
pub(crate) async fn before<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    user: UserId,
) -> Option<Before> {
    read(cx.cases(), user, cx.now()).await.ok()
}

async fn read(cases: &Cases, user: UserId, now: i64) -> Result<Before, CaseError> {
    let warnings = active_cases(cases, user, "warn", now, usize::MAX)
        .await?
        .len();
    let (counted, more) = count_cases(cases, user).await?;
    Ok(Before {
        warnings,
        cases: counted,
        more,
    })
}

/// Counts the cases of `user`, looking at no more than [`SCAN`] of them. `more` tells whether
/// there are cases beyond that.
async fn count_cases(cases: &Cases, user: UserId) -> Result<(usize, bool), CaseError> {
    let mut counted = 0;
    let mut before = None;
    while counted < SCAN {
        let limit = PAGE.min(SCAN - counted);
        let page = cases.for_target(user, before, limit).await?;
        counted += page.len();
        if page.len() < limit {
            return Ok((counted, false));
        }
        before = page.last().map(|case| case.number);
    }
    let older = cases.for_target(user, before, 1).await?;
    Ok((counted, !older.is_empty()))
}

/// The history line, in the language of the moderator.
pub(crate) fn line<I: InteractionRef + 'static>(cx: &Cx<I>, before: Before) -> String {
    let warnings = cx.t(
        "history-warnings",
        &Args::new().with("count", count(before.warnings)),
    );
    let cases = if before.more {
        cx.t(
            "history-cases-more",
            &Args::new().with("count", count(SCAN)),
        )
    } else {
        cx.t(
            "history-cases",
            &Args::new().with("count", count(before.cases)),
        )
    };
    cx.t(
        "history-line",
        &Args::new().with("warnings", warnings).with("cases", cases),
    )
}

#[cfg(test)]
mod tests {
    use closure_core::module::ModuleId;
    use twilight_model::id::Id;

    use super::*;

    #[tokio::test]
    async fn a_history_is_left_out_when_the_cases_cannot_be_read() {
        let cases = Cases::unavailable(ModuleId::new("moderation").unwrap(), Id::new(1));
        assert_eq!(read(&cases, Id::new(7), 0).await.ok(), None);
    }
}
