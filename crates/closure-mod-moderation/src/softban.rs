//! A softban: a ban that deletes the messages of the last days, lifted at once. The member is out
//! of the server and their messages are gone, and they may come back. It is one case of its own
//! kind (`softban`). The two steps are not left half done: if the unban fails after the ban got
//! through, the ban is recorded as the case, the moderator is told, and the member stays banned
//! until somebody lifts it.

use closure_core::action::Reason;
use closure_core::cases::{CaseKind, CaseSource, NewCase};
use closure_sdk::{Args, CommandCx, Tone, UserError, UserId};

use crate::{delete_seconds, mention, reason_of};

/// Bans, deletes the messages of `delete_days` (one by default), and lifts the ban.
pub(crate) async fn softban(
    cx: CommandCx,
    user: UserId,
    reason: Option<String>,
    delete_days: Option<i64>,
) -> Result<(), UserError> {
    cx.defer(true).await?;
    cx.check_target(user).await?;
    let days = delete_days.unwrap_or(1);
    let seconds = delete_seconds(days)?;
    let text = reason_of(reason.clone()).unwrap_or_else(|| {
        cx.ctx()
            .t_in(cx.ctx().locale, "softban-reason", &Args::new())
    });
    let reason = Reason::truncated(&text);
    let api = cx.api();
    api.ban(cx.guild_id(), user, seconds, reason.clone())
        .await?;
    // The unban is tried twice: a ban left standing is the one thing this command must not do.
    let mut lifted = api.unban(cx.guild_id(), user, reason.clone()).await;
    if lifted.is_err() {
        lifted = api.unban(cx.guild_id(), user, reason.clone()).await;
    }
    let stands = lifted.is_err();
    let kind = if stands { "ban" } else { "softban" };
    let recorded = cx
        .cases()
        .create(NewCase {
            source: CaseSource::Command,
            kind: CaseKind::new(kind).map_err(UserError::from)?,
            target: user,
            moderator: cx.user().id,
            reason: Some(reason.as_str().to_owned()),
            expires_at: None,
        })
        .await;
    let number = recorded.as_ref().map_or(0, |case| case.number);
    let said = Args::new()
        .with("user", mention(user))
        .with("number", number);
    if stands {
        return cx.say_private_t(Tone::Error, "softban-stuck", &said).await;
    }
    cx.say_private_t(Tone::Success, "softbanned", &said).await
}
