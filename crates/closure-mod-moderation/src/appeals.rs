//! Appeals: a member asks for a punishment to be lifted; a moderator may file one for them.
//! The appeal is a note on the member, posted to the approvals channel with undo and refuse buttons.
//! Each punishment is appealed once.

use closure_core::action::Action;
use closure_core::cases::{CaseKind, CaseSource, NewCase};
use closure_sdk::embed::{escape, truncate};
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{
    Args, CommandArgs, CommandCx, ComponentCx, ModuleError, Tone, UserError, UserId,
};
use twilight_model::channel::message::component::ButtonStyle;

use crate::templates::text_reply;
use crate::undo::{self, Kind};
use crate::{Config, active_cases, mention};

pub(crate) const ROUTE: &str = "appeal";
const APPEALED: &str = "appealed";
/// How much of the text the channel for approvals shows; the note keeps all of it.
const SHOWN: usize = 900;

#[derive(CommandArgs)]
pub(crate) struct Appeal {
    /// Why the punishment should be lifted
    // The note keeps the text whole, and a case reason holds 512 characters with the words around it.
    #[arg(max_length = 400)]
    text: String,
    /// The member, for a moderator who files an appeal that came another way
    user: Option<UserId>,
}

/// The newest punishment against `target` that still stands, whatever its kind.
async fn newest_standing(
    cx: &CommandCx,
    target: UserId,
) -> Result<Option<(Kind, closure_sdk::Case)>, UserError> {
    let now = cx.now();
    let mut newest: Option<(Kind, closure_sdk::Case)> = None;
    for kind in Kind::ALL {
        let found = active_cases(cx.cases(), target, kind.key(), now, 1).await?;
        if let Some(case) = found.into_iter().next()
            && newest
                .as_ref()
                .is_none_or(|(_, held)| case.number > held.number)
        {
            newest = Some((kind, case));
        }
    }
    Ok(newest)
}

#[allow(
    clippy::too_many_lines,
    reason = "one flow: check, mark, write the note, ask for approval"
)]
pub(crate) async fn file(cx: CommandCx, args: Appeal) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    let Some(channel) = config.approval_target() else {
        return Err(UserError::message("appeal-no-channel"));
    };
    let text = args.text.trim().to_owned();
    if text.is_empty() {
        return Err(UserError::message("appeal-empty"));
    }
    let target = match args.user {
        Some(user) if user != cx.user().id => {
            cx.require("moderation.appeals").await?;
            user
        }
        _ => cx.user().id,
    };
    let Some((kind, case)) = newest_standing(&cx, target).await? else {
        return Err(UserError::message("appeal-nothing"));
    };
    // Once for each punishment: a second try is not an appeal but a nuisance.
    let first = cx
        .ctx()
        .store()
        .set_add(APPEALED, u64::try_from(case.number).unwrap_or(0))
        .await
        .map_err(|error| UserError::from(ModuleError::other(error.to_string())))?;
    if !first {
        return Err(UserError::message("appeal-again").with("number", case.number));
    }
    let note = cx
        .cases()
        .create(NewCase {
            source: CaseSource::Appeal,
            kind: CaseKind::new("note").map_err(|_| UserError::message("appeal-nothing"))?,
            target,
            moderator: cx.user().id,
            reason: Some(truncate(
                &cx.t(
                    "appeal-note",
                    &Args::new()
                        .with("number", case.number)
                        .with("text", text.clone()),
                ),
                closure_core::cases::MAX_REASON_CHARS,
            )),
            expires_at: None,
        })
        .await;
    let note = match note {
        Ok(note) => note,
        Err(error) => {
            // The appeal was not written down: the punishment may be appealed again.
            let _ = cx
                .ctx()
                .store()
                .set_remove(APPEALED, u64::try_from(case.number).unwrap_or(0))
                .await;
            return Err(error.into());
        }
    };
    let _ = cx.modlog(&note).await;
    let deny = CustomId::new(ROUTE)
        .data(format!("deny:{}:{}", target.get(), case.number))
        .encode(cx.module());
    let components = vec![ui::row([
        undo::button(&cx, kind, target, case.number),
        ui::button(ButtonStyle::Danger, cx.t("appeal-deny", &Args::new()), deny),
    ])];
    let post = cx
        .api()
        .execute(Action::PostMessage {
            channel_id: channel,
            message_id: None,
            reply_to: None,
            content: Some(
                cx.t(
                    "appeal-asked",
                    &Args::new()
                        .with("user", mention(target))
                        .with("number", case.number)
                        .with("text", truncate(&escape(&text), SHOWN)),
                ),
            ),
            embeds: Vec::new(),
            components,
            files: Vec::new(),
        })
        .await;
    if post.is_err() {
        // The staff never saw it, so the member may try again.
        let _ = cx
            .ctx()
            .store()
            .set_remove(APPEALED, u64::try_from(case.number).unwrap_or(0))
            .await;
        return Err(UserError::message("appeal-not-sent"));
    }
    cx.say_private_t(
        Tone::Success,
        "appeal-sent",
        &Args::new().with("number", case.number),
    )
    .await
}

/// A moderator refused the appeal. The punishment stays; the message says who decided.
pub(crate) async fn press(cx: ComponentCx) -> Result<(), UserError> {
    cx.require("moderation.appeals").await?;
    let data = cx.data();
    let mut parts = data.splitn(3, ':');
    let (Some("deny"), Some(user), Some(number)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(UserError::message("undo-not-needed"));
    };
    let user: u64 = user
        .parse()
        .map_err(|_| UserError::message("undo-not-needed"))?;
    cx.update(text_reply(
        cx.t(
            "appeal-denied",
            &Args::new()
                .with("user", format!("<@{user}>"))
                .with("number", number)
                .with("moderator", mention(cx.user().id)),
        ),
    )?)
    .await
}
