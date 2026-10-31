//! Posts a moderation case to the log channel of its server, as an embed in the server's
//! language: the log is read by the moderators, not by whoever ran the command.

use closure_core::action::{Action, MentionPolicy, Outcome};
use closure_core::cases::Case;
use closure_core::error::DiscordError;
use closure_core::event::MessageId;
use closure_core::locale::Args;
use closure_core::module::EventContext;

use crate::UserError;
use crate::duration::{TimeStyle, timestamp};
use crate::embed::{Color, EmbedBuilder, FIELD_VALUE_LIMIT, escape, truncate};

/// The kinds the catalog names (`modlog-kind-<kind>`). Any other kind is shown as it is.
const KNOWN_KINDS: [&str; 9] = [
    "warn",
    "timeout",
    "untimeout",
    "kick",
    "ban",
    "unban",
    "note",
    "vcmute",
    "vcunmute",
];

/// Grey, for a case that was revoked whatever its kind.
const REVOKED_COLOR: Color = Color::Rgb(0x0095_A5A6);

/// Posts `case` to the log channel of the guild and returns the id of the message. Returns
/// `None` and sends nothing when the guild has no log channel. Needs the capability
/// `SendMessages` as well as `cases`.
pub async fn post(ctx: &EventContext, case: &Case) -> Result<Option<MessageId>, UserError> {
    let Some(channel) = ctx.policy().log_channel else {
        return Ok(None);
    };
    let message = embed(ctx, case).build()?;
    let outcome = ctx
        .api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: None,
            embeds: vec![message],
            reply_to: None,
            mentions: MentionPolicy::None,
        })
        .await?;
    match outcome {
        Outcome::Message(message) => Ok(Some(message.id)),
        _ => Err(DiscordError::Fatal("unexpected outcome for `send_message`".into()).into()),
    }
}

/// The case as the log shows it, in the server's language. A reply that shows a case uses it too.
#[must_use]
pub fn embed(ctx: &EventContext, case: &Case) -> EmbedBuilder {
    let locale = ctx.locale;
    let label = |key: &str| ctx.t_core(locale, key, &Args::new());
    let kind = kind_name(ctx, case.kind.as_str());
    let title = ctx.t_core(
        locale,
        "modlog-title",
        &Args::new().with("number", case.number).with("kind", kind),
    );
    let mut builder = EmbedBuilder::new()
        .title(title)
        .color(color(case.kind.as_str(), case.revoked.is_some()))
        .footer(case.module.as_str())
        .timestamp(case.created_at)
        .field(
            label("modlog-target"),
            format!("<@{0}> ({0})", case.target),
            true,
        )
        .field(
            label("modlog-moderator"),
            format!("<@{0}> ({0})", case.moderator),
            true,
        );
    let reason = match case.reason.as_deref().filter(|reason| !reason.is_empty()) {
        Some(reason) => truncate(&escape(reason), FIELD_VALUE_LIMIT),
        None => label("modlog-no-reason"),
    };
    builder = builder.field(label("modlog-reason"), reason, false);
    if let Some(expires) = case.expires_at {
        let value = format!(
            "{} ({})",
            timestamp(expires, TimeStyle::Relative),
            timestamp(expires, TimeStyle::LongDateTime)
        );
        builder = builder.field(label("modlog-expires"), value, false);
    }
    if let Some(revoked) = &case.revoked {
        let head = ctx.t_core(
            locale,
            "modlog-revoked-by",
            &Args::new()
                .with("who", format!("<@{}>", revoked.by))
                .with("when", timestamp(revoked.at, TimeStyle::Relative)),
        );
        // The reason shares the field with the line above it, so it gets what is left.
        let value = match revoked
            .reason
            .as_deref()
            .filter(|reason| !reason.is_empty())
        {
            Some(reason) => {
                let room = FIELD_VALUE_LIMIT.saturating_sub(head.chars().count() + 1);
                format!("{head}\n{}", truncate(&escape(reason), room))
            }
            None => head,
        };
        builder = builder.field(label("modlog-revoked"), value, false);
    }
    builder
}

pub(crate) fn kind_name(ctx: &EventContext, kind: &str) -> String {
    if KNOWN_KINDS.contains(&kind) {
        ctx.t_core(ctx.locale, &format!("modlog-kind-{kind}"), &Args::new())
    } else {
        kind.to_owned()
    }
}

pub(crate) fn color(kind: &str, revoked: bool) -> Color {
    if revoked {
        return REVOKED_COLOR;
    }
    match kind {
        "warn" | "timeout" | "kick" | "vcmute" => Color::Warning,
        "ban" => Color::Error,
        "unban" | "untimeout" | "vcunmute" => Color::Success,
        _ => Color::Info,
    }
}
