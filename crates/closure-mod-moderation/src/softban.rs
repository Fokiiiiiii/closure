//! A softban: a ban that deletes recent messages and is lifted at once (case kind `softban`).
//! If the unban fails after the ban, the ban is recorded and the moderator told; the member stays banned.

use closure_core::action::Reason;
use closure_core::cases::{CaseKind, CaseSource, NewCase};
use closure_sdk::{Args, Cx, InteractionRef, Tone, UserError, UserId, punish};

use crate::{delete_seconds, mention, reason_of};

pub(crate) async fn softban<I: InteractionRef + 'static>(
    cx: Cx<I>,
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
    // Nothing else is done to the member meanwhile, and the intent is saved first: a stop after the
    // ban is finished by lifting it (see `closure_sdk::punish::remember_softban`).
    let _turn = punish::member_turn(cx.ctx(), user).await;
    punish::remember_softban(cx.ctx(), cx.user().id, user, Some(reason.as_str())).await?;
    if let Err(error) = api.ban(cx.guild_id(), user, seconds, reason.clone()).await {
        punish::forget_softban(cx.ctx(), user).await?;
        return Err(error.into());
    }
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
    // Kept when the case could not be made: the recovery makes it.
    if recorded.is_ok() {
        punish::forget_softban(cx.ctx(), user).await?;
    }
    let number = recorded.as_ref().map_or(0, |case| case.number);
    let said = Args::new()
        .with("user", mention(user))
        .with("number", number);
    if stands {
        return cx.say_private_t(Tone::Error, "softban-stuck", &said).await;
    }
    cx.say_private_t(Tone::Success, "softbanned", &said).await
}
