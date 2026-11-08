//! Posts a moderation case to the log channel of its server, as an embed in the server's
//! language: the log is read by the moderators, not by whoever ran the command.

use closure_core::action::Outcome;
use closure_core::cases::Case;
use closure_core::error::DiscordError;
use closure_core::event::MessageId;
use closure_core::locale::Args;
use closure_core::module::EventContext;
use serde::{Deserialize, Serialize};

use crate::UserError;
use crate::duration::{TimeStyle, timestamp};
use crate::embed::{Color, EmbedBuilder, FIELD_VALUE_LIMIT, escape, truncate};
use crate::files::{self, Destination};

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

/// Sends a case-free log message, or does nothing when no log destination is configured.
pub async fn post_text(ctx: &EventContext, content: String) -> Result<(), UserError> {
    if let Some(destination) = Destination::from_policy(ctx.policy()) {
        files::send(ctx, &destination, Some(content), Vec::new(), None).await?;
    }
    Ok(())
}

/// Posts `case` to the guild's log destination and returns its message id; `None` when there is none.
pub async fn post(ctx: &EventContext, case: &Case) -> Result<Option<MessageId>, UserError> {
    let Some(destination) = Destination::from_policy(ctx.policy()) else {
        return Ok(None);
    };
    post_to(ctx, case, &destination).await
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Pending {
    Legacy(i64),
    Destination {
        number: i64,
        destination: Destination,
    },
}

async fn post_to(
    ctx: &EventContext,
    case: &Case,
    destination: &Destination,
) -> Result<Option<MessageId>, UserError> {
    let key = format!("modlog-pending:{}", case.number);
    let pending = Pending::Destination {
        number: case.number,
        destination: destination.clone(),
    };
    let durable = match ctx.store().set(&key, &pending).await {
        Ok(()) => true,
        Err(closure_core::storage::StorageError::NotGranted) => false,
        Err(error) => return Err(error.into()),
    };
    if durable {
        // The set is what a retry works through, so a case that keeps failing cannot hold the others back.
        ctx.store().set_add(PENDING_SET, case_number(case)).await?;
    }
    let message = embed(ctx, case).build()?;
    let outcome = match files::send(ctx, destination, None, vec![message], None).await {
        Err(error @ (DiscordError::WebhookNotRegistered | DiscordError::WebhookGone)) => {
            if durable {
                settle(ctx, case).await?;
            }
            return Err(error.into());
        }
        other => other?,
    };
    match outcome {
        Outcome::Message(message) => {
            if durable {
                settle(ctx, case).await?;
            }
            Ok(Some(message.id))
        }
        _ => Err(DiscordError::Fatal("unexpected outcome for `send_message`".into()).into()),
    }
}

/// The set of the numbers of the cases whose log post is not done.
const PENDING_SET: &str = "modlog-pending";
/// Where the walk through the set stopped the last time.
const CURSOR_KEY: &str = "modlog-cursor";

fn case_number(case: &Case) -> u64 {
    u64::try_from(case.number).unwrap_or(0)
}

/// A case's log post is done or given up: it is no longer pending.
async fn settle(ctx: &EventContext, case: &Case) -> Result<(), UserError> {
    ctx.store()
        .delete(&format!("modlog-pending:{}", case.number))
        .await?;
    ctx.store()
        .set_remove(PENDING_SET, case_number(case))
        .await?;
    Ok(())
}

/// Retries up to `limit` pending log posts. Failed ones go to the end so they do not block the rest.
pub(crate) async fn retry_pending(ctx: &EventContext, limit: usize) -> Result<(), UserError> {
    // Entries written before the set existed are added to it as they are met.
    for (key, entry) in ctx.store().list("modlog-pending:", limit).await? {
        if let Some(number) = number_of(&entry.value) {
            ctx.store().set_add(PENDING_SET, number).await?;
        } else {
            tracing::warn!(%key, "a pending case log has no case number");
        }
    }
    // A cursor walks the set, so posts that keep failing do not hold back the others.
    let cursor: u64 = ctx.store().get(CURSOR_KEY).await?.unwrap_or(0);
    let mut batch = ctx.store().set_after(PENDING_SET, cursor, limit).await?;
    if batch.is_empty() && cursor != 0 {
        // The end of the set: the walk starts again at its beginning.
        batch = ctx.store().set_after(PENDING_SET, 0_u64, limit).await?;
    }
    let next = batch.last().copied().unwrap_or(0);
    ctx.store().set(CURSOR_KEY, &next).await?;
    for number in batch {
        let key = format!("modlog-pending:{number}");
        let result: Result<(), UserError> = async {
            let Some(entry) = ctx.store().entry(&key).await? else {
                return Ok(());
            };
            let pending: Pending = serde_json::from_value(entry.value)
                .map_err(|error| closure_core::storage::StorageError::Value(error.to_string()))?;
            let (number, destination) = match pending {
                Pending::Legacy(number) => (number, Destination::from_policy(ctx.policy())),
                Pending::Destination {
                    number,
                    destination,
                } => (number, Some(destination)),
            };
            if let Some(case) = ctx.cases().get(number).await? {
                if let Some(destination) = destination {
                    post_to(ctx, &case, &destination).await?;
                } else {
                    settle(ctx, &case).await?;
                }
            } else {
                ctx.store().delete(&key).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            // It stays in the set: it is tried again when the walk comes round to it.
            tracing::warn!(%key, ?error, "could not retry a case log");
        } else if ctx.store().entry(&key).await?.is_none() {
            ctx.store().set_remove(PENDING_SET, number).await?;
        }
    }
    Ok(())
}

/// The case number a pending entry names.
fn number_of(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.get("number").and_then(serde_json::Value::as_u64))
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
