//! The moderation menu: a context command on a member opens buttons, which open forms.
//! form that runs the action; every step checks the action's permission again.

use std::time::Duration;

use closure_sdk::ui::{self, CustomId, text_input};
use closure_sdk::{
    Args, ComponentCx, ContextCommand, ContextCx, InteractionReply, ModalCx, ModuleBuilder, Span,
    UserError, UserId,
};
use twilight_model::channel::message::component::ButtonStyle;

use crate::{
    BanArgs, MemberReason, Note, TimeoutArgs, Warn, ban, case_list, kick, mention, note, timeout,
    warn,
};

const PICK: &str = "menu";
const DO: &str = "menu_do";
/// How long the menu keeps working after it was opened.
const LIFETIME: Duration = Duration::from_mins(15);
/// The longest length of text typed for a duration, and for a number of days.
const SPAN_CHARS: u16 = 32;
const REASON_CHARS: u16 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Warn,
    Note,
    Timeout,
    Kick,
    Ban,
    Cases,
}

impl Action {
    const ALL: [Self; 6] = [
        Self::Warn,
        Self::Note,
        Self::Timeout,
        Self::Kick,
        Self::Ban,
        Self::Cases,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Note => "note",
            Self::Timeout => "timeout",
            Self::Kick => "kick",
            Self::Ban => "ban",
            Self::Cases => "cases",
        }
    }

    fn named(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.key() == key)
    }

    fn permission(self) -> &'static str {
        match self {
            Self::Warn => "moderation.warn",
            Self::Note => "moderation.note",
            Self::Timeout => "moderation.timeout",
            Self::Kick => "moderation.kick",
            Self::Ban => "moderation.ban",
            Self::Cases => "moderation.cases",
        }
    }

    fn style(self) -> ButtonStyle {
        match self {
            Self::Kick | Self::Ban => ButtonStyle::Danger,
            Self::Timeout => ButtonStyle::Primary,
            Self::Warn | Self::Note | Self::Cases => ButtonStyle::Secondary,
        }
    }
}

/// `action:member`, as the buttons and the forms carry it.
fn data_of(action: Action, user: UserId) -> String {
    format!("{}:{}", action.key(), user.get())
}

fn read_data(data: &str) -> Result<(Action, UserId), UserError> {
    let gone = || UserError::message("menu-gone");
    let (action, user) = data.split_once(':').ok_or_else(gone)?;
    let action = Action::named(action).ok_or_else(gone)?;
    let user = user
        .parse::<u64>()
        .ok()
        .and_then(UserId::new_checked)
        .ok_or_else(gone)?;
    Ok((action, user))
}

pub(crate) fn register(builder: ModuleBuilder) -> ModuleBuilder {
    builder
        .context(
            ContextCommand::user("Moderate")
                .requires("moderation.note")
                .handler(open),
        )
        .component(PICK, pick)
        .modal(DO, submit)
}

async fn open(cx: ContextCx) -> Result<(), UserError> {
    let Some(user) = cx.target_user().map(|user| user.id) else {
        return Err(UserError::message("menu-no-member"));
    };
    // Refused here what the actions would refuse anyway: oneself, a bot, somebody above.
    cx.check_target(user).await?;
    let buttons: Vec<_> = Action::ALL
        .into_iter()
        .map(|action| {
            let custom_id = CustomId::new(PICK)
                .only_for(cx.user().id)
                .expires_in(cx.now(), LIFETIME)
                .data(data_of(action, user))
                .encode(cx.module());
            ui::button(
                action.style(),
                cx.t(&format!("menu-button-{}", action.key()), &Args::new()),
                custom_id,
            )
        })
        .collect();
    let (first, second) = buttons.split_at(5);
    cx.reply_with(InteractionReply {
        content: Some(cx.t("menu-title", &Args::new().with("user", mention(user)))),
        components: vec![
            ui::row(first.iter().cloned()),
            ui::row(second.iter().cloned()),
        ],
        ephemeral: true,
        ..InteractionReply::default()
    })
    .await
}

/// A button: the cases are listed at once, the other actions ask for their details in a form.
async fn pick(cx: ComponentCx) -> Result<(), UserError> {
    let (action, user) = read_data(&cx.data())?;
    cx.require(action.permission()).await?;
    if action == Action::Cases {
        return case_list(cx, user, None).await;
    }
    let module = cx.module().clone();
    let label = |key: &str| cx.t(key, &Args::new());
    let mut inputs = Vec::new();
    match action {
        Action::Note => inputs.push(text_input(
            &module,
            "text",
            &label("menu-field-note"),
            true,
            true,
            REASON_CHARS,
        )),
        Action::Timeout => {
            inputs.push(text_input(
                &module,
                "duration",
                &label("menu-field-duration"),
                false,
                true,
                SPAN_CHARS,
            ));
            inputs.push(text_input(
                &module,
                "reason",
                &label("menu-field-reason"),
                true,
                false,
                REASON_CHARS,
            ));
        }
        Action::Ban => {
            inputs.push(text_input(
                &module,
                "reason",
                &label("menu-field-reason"),
                true,
                false,
                REASON_CHARS,
            ));
            inputs.push(text_input(
                &module,
                "duration",
                &label("menu-field-ban-duration"),
                false,
                false,
                SPAN_CHARS,
            ));
            inputs.push(text_input(
                &module,
                "days",
                &label("menu-field-delete-days"),
                false,
                false,
                1,
            ));
        }
        Action::Warn | Action::Kick | Action::Cases => inputs.push(text_input(
            &module,
            "reason",
            &label("menu-field-reason"),
            true,
            false,
            REASON_CHARS,
        )),
    }
    let custom_id = CustomId::new(DO)
        .only_for(cx.user().id)
        .data(data_of(action, user))
        .encode(&module);
    let title = cx.t(
        &format!("menu-modal-{}", action.key()),
        &Args::new().with("user", mention(user)),
    );
    cx.show_modal(ui::modal(custom_id, title, inputs)).await
}

async fn submit(cx: ModalCx) -> Result<(), UserError> {
    let (action, user) = CustomId::parse(&cx.submission().custom_id)
        .map(|(_, id)| id.data)
        .ok_or_else(|| UserError::message("menu-gone"))
        .and_then(|data| read_data(&data))?;
    cx.require(action.permission()).await?;
    let text = |name: &str| {
        cx.field(name)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    match action {
        Action::Warn => {
            warn(
                cx.clone(),
                Warn {
                    user,
                    reason: text("reason"),
                },
            )
            .await
        }
        Action::Note => {
            let Some(body) = text("text") else {
                return Err(UserError::message("menu-note-empty"));
            };
            note(cx.clone(), Note { user, text: body }).await
        }
        Action::Timeout => {
            let duration = text("duration")
                .ok_or_else(|| UserError::message("duration-required"))
                .and_then(|text| {
                    Span::parse(&text).map_err(|_| UserError::message("menu-bad-duration"))
                })?;
            timeout(
                cx.clone(),
                TimeoutArgs {
                    user,
                    duration: Some(duration),
                    reason: text("reason"),
                },
            )
            .await
        }
        Action::Kick => {
            kick(
                cx.clone(),
                MemberReason {
                    user,
                    reason: text("reason"),
                },
            )
            .await
        }
        Action::Ban => {
            let duration = text("duration")
                .map(|text| Span::parse(&text).map_err(|_| UserError::message("menu-bad-duration")))
                .transpose()?;
            let delete_days = text("days")
                .map(|text| {
                    text.parse::<i64>()
                        .ok()
                        .filter(|days| (0..=7).contains(days))
                        .ok_or_else(|| UserError::message("menu-bad-days"))
                })
                .transpose()?;
            ban(
                cx.clone(),
                BanArgs {
                    user,
                    reason: text("reason"),
                    duration,
                    delete_days,
                    soft: None,
                },
            )
            .await
        }
        Action::Cases => Err(UserError::message("menu-gone")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_button_carries_is_read_back_and_nonsense_is_refused() {
        let user = UserId::new(42);
        for action in Action::ALL {
            assert_eq!(read_data(&data_of(action, user)).unwrap(), (action, user));
        }
        for bad in ["", "ban", "ban:", "ban:x", "ban:0", "nuke:42", ":42"] {
            assert!(read_data(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn every_action_has_a_permission_the_module_declares() {
        for action in Action::ALL {
            assert!(action.permission().starts_with("moderation."));
        }
    }
}
