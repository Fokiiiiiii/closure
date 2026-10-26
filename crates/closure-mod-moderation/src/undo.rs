//! Undo buttons: the button a punishment's reply carries lifts the punishment again, as its
//! un-command does. The button is not `only_for`, so any moderator may press it. Each press checks
//! the permission of the kind and the target again, and that the case still stands.
//!
//! A button never expires by itself, since the SDK has no expiry for components yet. The check that
//! the case still stands is what keeps an old button harmless.
//!
//! The button of an older punishment while a newer one still stands only revokes its own case:
//! the newer one is still in force, so Discord and the timers are left alone.

use closure_sdk::ui::{self, CustomId};
use closure_sdk::{Args, Case, CaseError, ComponentCx, Cx, InteractionRef, UserError, UserId};
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::ButtonStyle;

use crate::templates::text_reply;
use crate::{Config, mention, punisher_for, stands};

/// The route of the undo buttons.
pub(crate) const ROUTE: &str = "undo";

/// The punishments a button undoes. A kick has nothing to undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Timeout,
    Ban,
    Vcmute,
    Warn,
}

impl Kind {
    /// The kind of the case the button undoes, as the custom id and the cases name it.
    fn key(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Ban => "ban",
            Self::Vcmute => "vcmute",
            Self::Warn => "warn",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [Self::Timeout, Self::Ban, Self::Vcmute, Self::Warn]
            .into_iter()
            .find(|kind| kind.key() == text)
    }

    /// The permission that the un-command of the kind checks, and that a press checks again.
    fn permission(self) -> &'static str {
        match self {
            Self::Timeout => "moderation.timeout",
            Self::Ban => "moderation.ban",
            Self::Vcmute => "moderation.voice",
            Self::Warn => "moderation.revoke",
        }
    }

    fn label_key(self) -> &'static str {
        match self {
            Self::Warn => "undo-button-warn",
            Self::Timeout | Self::Ban | Self::Vcmute => "undo-button",
        }
    }
}

/// The undo button of case `number`, a punishment of `kind` against `user`.
pub(crate) fn button<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    kind: Kind,
    user: UserId,
    number: i64,
) -> Component {
    let custom_id = CustomId::new(ROUTE)
        .data(format!("{}:{}:{number}", kind.key(), user.get()))
        .encode(cx.module());
    ui::button(
        ButtonStyle::Secondary,
        cx.t(kind.label_key(), &Args::new()),
        custom_id,
    )
}

/// The row that holds the undo button, or no row when there is no button.
pub(crate) fn row(button: Option<Component>) -> Vec<Component> {
    button.map_or_else(Vec::new, |button| vec![ui::row([button])])
}

/// The kind, member and case that the data of a button names.
fn parse(data: &str) -> Option<(Kind, UserId, i64)> {
    let mut parts = data.splitn(3, ':');
    let kind = Kind::parse(parts.next()?)?;
    let user = UserId::new_checked(parts.next()?.parse().ok()?)?;
    let number = parts.next()?.parse().ok()?;
    Some((kind, user, number))
}

fn not_needed() -> UserError {
    UserError::message("undo-not-needed")
}

/// Refuses unless case `number` is a punishment of `kind` against `user` that still stands.
async fn ensure_standing<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    kind: Kind,
    user: UserId,
    number: i64,
) -> Result<(), UserError> {
    match cx.cases().get(number).await? {
        Some(case) if case.target == user && stands(&case, kind.key(), cx.now()) => Ok(()),
        _ => Err(not_needed()),
    }
}

/// A moderator pressed an undo button. The punishment is lifted as its un-command lifts it, and
/// the message is replaced by the result. A press that is not needed changes nothing.
pub(crate) async fn press(cx: ComponentCx) -> Result<(), UserError> {
    let (kind, user, number) = parse(&cx.data()).ok_or_else(not_needed)?;
    cx.require(kind.permission()).await?;
    cx.check_target(user).await?;
    ensure_standing(&cx, kind, user, number).await?;
    let config: Config = cx.config()?;
    // Discord is called next, so the click is acknowledged first.
    cx.defer_update().await?;
    let punisher = punisher_for(&cx, &config).undoing(number);
    let body = match kind {
        Kind::Timeout => {
            let lifted = punisher
                .lift_timeout(user, Some(undo_reason(&cx, number)))
                .await?;
            lift_body(&cx, number, &lifted, user, "untimed-out").await?
        }
        Kind::Ban => {
            let lifted = punisher.unban(user, Some(undo_reason(&cx, number))).await?;
            lift_body(&cx, number, &lifted, user, "unbanned").await?
        }
        Kind::Vcmute => {
            let lifted = punisher
                .voice_unmute(user, Some(undo_reason(&cx, number)))
                .await?;
            lift_body(&cx, number, &lifted, user, "voice-unmuted").await?
        }
        // A warning is revoked, as `/case revoke` revokes it: no action on Discord.
        Kind::Warn => {
            revoke(&cx, number).await?;
            cx.t("case-revoked", &Args::new().with("number", number))
        }
    };
    cx.update(text_reply(undone_by(&cx, &body))?).await
}

fn undo_reason<I: InteractionRef + 'static>(cx: &Cx<I>, number: i64) -> String {
    cx.t("undo-reason", &Args::new().with("case", number))
}

/// The result of a lift. A lift that a newer punishment still holds returns the case it was asked
/// to undo, still standing: nothing was lifted, so only that case is revoked.
async fn lift_body<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    number: i64,
    lifted: &Case,
    user: UserId,
    lifted_key: &str,
) -> Result<String, UserError> {
    // A lift records a new case, so the number is the undone one only when nothing was lifted.
    if lifted.number == number {
        revoke(cx, number).await?;
        return Ok(cx.t("undo-superseded", &Args::new().with("number", number)));
    }
    Ok(cx.t(
        lifted_key,
        &Args::new()
            .with("user", mention(user))
            .with("number", lifted.number),
    ))
}

/// Revokes case `number` as a button does, and posts the revocation to the log.
async fn revoke<I: InteractionRef + 'static>(cx: &Cx<I>, number: i64) -> Result<(), UserError> {
    let revoked = match cx
        .cases()
        .revoke(
            number,
            cx.user().id,
            Some(cx.t("undone-by-button", &Args::new())),
        )
        .await
    {
        Ok(case) => case,
        Err(CaseError::AlreadyRevoked | CaseError::NotFound) => return Err(not_needed()),
        Err(error) => return Err(error.into()),
    };
    let _ = cx.modlog(&revoked).await;
    Ok(())
}

/// The result, with the line that says who undid the punishment.
fn undone_by<I: InteractionRef + 'static>(cx: &Cx<I>, body: &str) -> String {
    format!(
        "{}\n{body}",
        cx.t("undo-by", &Args::new().with("user", mention(cx.user().id)))
    )
}
