//! The logs module for closure: what happens to the messages and members of a server, posted to a
//! channel. A deleted message is shown with what the history kept of it: its text, and its files when
//! the host kept them. The files and their posting are the SDK's (`closure_sdk::files`).

use std::collections::HashSet;

use closure_core::capability::Capability;
use closure_core::error::{CoreError, ModuleError};
use closure_core::event::{ChannelId, Event, EventKind, MessageId, MessageInfo, UserInfo};
use closure_core::history::SavedMessage;
use closure_core::locale::Locale;
use closure_core::module::EventContext;
use closure_core::subscription::{Flow, Subscription};
use closure_sdk::files::{self, Kept, LogEntry};
use closure_sdk::{
    Args, Color, EmbedBuilder, Intents, ModuleBuilder, SdkModule, parse_module_config,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::channel::message::Embed;
use twilight_model::id::Id;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

/// Discord takes at most this many characters in one embed field value.
const FIELD_LIMIT: usize = 1024;

/// The module's section of a guild's configuration. One switch per kind of entry, as the
/// configuration names them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Deserialize)]
struct Config {
    /// The channel the entries are posted to; `None` takes the guild's log channel.
    channel: Option<String>,
    message_edits: bool,
    message_deletes: bool,
    member_joins: bool,
    member_leaves: bool,
    bans: bool,
}

fn settings() -> (serde_json::Value, serde_json::Value) {
    let schema = json!({
        "type": "object",
        "properties": {
            "channel": {
                "anyOf": [{ "type": "string", "pattern": "^[0-9]+$" }, { "type": "null" }],
                "description": "The channel the entries are posted to. Left out, the guild's log channel.",
                "x-ui": { "level": "basic" }
            },
            "message_edits": { "type": "boolean", "description": "Post the edits of messages." },
            "message_deletes": {
                "type": "boolean",
                "description": "Post the deletions of messages, with their text and files as the history kept them.",
                "x-ui": { "level": "basic" }
            },
            "member_joins": { "type": "boolean", "description": "Post the joins of members." },
            "member_leaves": { "type": "boolean", "description": "Post the departures of members." },
            "bans": { "type": "boolean", "description": "Post the bans and unbans of members." },
            "keep_attachments": {
                "type": "boolean",
                "description": "Keep the files of attachments as their messages arrive, not only when a message is deleted or edited. Uses storage: a server's files are capped and kept for the message history's retention.",
                "x-ui": { "level": "basic" }
            }
        },
        "additionalProperties": false
    });
    let defaults = json!({
        "channel": null,
        "message_edits": true,
        "message_deletes": true,
        "member_joins": true,
        "member_leaves": true,
        "bans": true,
        "keep_attachments": false
    });
    (schema, defaults)
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = settings();
    Ok(
        ModuleBuilder::new("logs", "Logs", env!("CARGO_PKG_VERSION"))?
            .description("Post the edits and deletions of messages, and the joins, leaves and bans of members, to a channel.")
            .capability(Capability::SendMessages)
            .capability(Capability::ReadMessageHistory)
            .capability(Capability::KeepAttachments)
            .intents(Intents::GUILD_MESSAGES | Intents::GUILD_MEMBERS | Intents::GUILD_MODERATION)
            .on(Subscription::new(EventKind::MessageUpdated), on_event)
            .on(Subscription::new(EventKind::MessageDeleted), on_event)
            .on(Subscription::new(EventKind::MessagesBulkDeleted), on_event)
            .on(Subscription::new(EventKind::MemberJoined), on_event)
            .on(Subscription::new(EventKind::MemberLeft), on_event)
            .on(Subscription::new(EventKind::BanAdded), on_event)
            .on(Subscription::new(EventKind::BanRemoved), on_event)
            .config(schema, defaults)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

async fn on_event(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let config: Config = parse_module_config(&ctx)?;
    let Some(channel) = destination(&ctx, &config) else {
        return Ok(Flow::Continue);
    };
    let entries = match &event {
        Event::MessageUpdated(message) if config.message_edits => {
            vec![edited(&ctx, message).await?]
        }
        Event::MessageDeleted {
            channel_id,
            message_id,
            ..
        } if config.message_deletes => vec![deleted(&ctx, *channel_id, *message_id).await?],
        Event::MessagesBulkDeleted {
            channel_id,
            message_ids,
            ..
        } if config.message_deletes => bulk_deleted(&ctx, *channel_id, message_ids).await?,
        Event::MemberJoined(member) if config.member_joins => {
            vec![member_entry(
                &ctx,
                "logs-joined",
                Color::Success,
                &member.user,
            )?]
        }
        Event::MemberLeft { user, .. } if config.member_leaves => {
            vec![member_entry(&ctx, "logs-left", Color::Info, user)?]
        }
        Event::BanAdded { user, .. } if config.bans => {
            vec![member_entry(&ctx, "logs-banned", Color::Error, user)?]
        }
        Event::BanRemoved { user, .. } if config.bans => {
            vec![member_entry(&ctx, "logs-unbanned", Color::Success, user)?]
        }
        _ => return Ok(Flow::Continue),
    };
    files::post(&ctx, channel, entries).await?;
    Ok(Flow::Continue)
}

/// The channel the entries go to: the one the module is configured with, else the guild's log channel.
fn destination(ctx: &EventContext, config: &Config) -> Option<ChannelId> {
    config
        .channel
        .as_deref()
        .and_then(|text| text.parse::<u64>().ok())
        .and_then(Id::new_checked)
        .or(ctx.policy().log_channel)
}

async fn deleted(
    ctx: &EventContext,
    channel: ChannelId,
    message: MessageId,
) -> Result<LogEntry, ModuleError> {
    let saved = ctx
        .history()
        .get(channel, message)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let kept = files::kept_files(ctx, message).await?;
    let card = match &saved {
        Some(saved) => saved_card(ctx, "logs-deleted", Color::Error, saved, &kept),
        None => unkept_card(ctx, channel.get(), message.get()),
    };
    Ok(LogEntry {
        embed: build(card)?,
        files: kept.iter().filter_map(Kept::upload).collect(),
    })
}

/// The deletions of a bulk delete: an entry for each message the history kept, and a summary that
/// counts the messages and the ones whose text was not kept.
async fn bulk_deleted(
    ctx: &EventContext,
    channel: ChannelId,
    messages: &[MessageId],
) -> Result<Vec<LogEntry>, ModuleError> {
    let mut entries = Vec::new();
    let mut unkept = 0_u64;
    for message in messages {
        let saved = ctx
            .history()
            .get(channel, *message)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
        match saved {
            Some(_) => entries.push(deleted(ctx, channel, *message).await?),
            None => unkept += 1,
        }
    }
    let count = u64::try_from(messages.len()).unwrap_or(u64::MAX);
    let mut summary = card(
        ctx.t_in(ctx.locale, "logs-bulk", &Args::new().with("count", count)),
        Color::Error,
    );
    if unkept > 0 {
        let note = ctx.t_in(
            ctx.locale,
            "logs-bulk-unknown",
            &Args::new().with("count", unkept),
        );
        summary = summary.field(label(ctx, "logs-content"), note, false);
    }
    entries.insert(
        0,
        LogEntry {
            embed: build(summary)?,
            files: Vec::new(),
        },
    );
    Ok(entries)
}

/// An edit: the new text and files, and the files the edit removed, which the history kept.
async fn edited(ctx: &EventContext, message: &MessageInfo) -> Result<LogEntry, ModuleError> {
    let current: HashSet<u64> = message.attachments.iter().map(|file| file.id).collect();
    let removed: Vec<Kept> = files::kept_files(ctx, message.id)
        .await?
        .into_iter()
        .filter(|kept| !current.contains(&kept.id))
        .collect();
    let mut card = message_card(
        ctx,
        label(ctx, "logs-edited"),
        Color::Warning,
        message.author.id.get(),
        message.channel_id.get(),
        &message.content,
        &message.attachments,
    );
    if !removed.is_empty() {
        card = card.field(label(ctx, "logs-removed"), names(ctx, &removed), false);
    }
    Ok(LogEntry {
        embed: build(card)?,
        files: removed.iter().filter_map(Kept::upload).collect(),
    })
}

/// A saved message with its author, channel, text and the files that were kept of it.
fn saved_card(
    ctx: &EventContext,
    title: &str,
    color: Color,
    saved: &SavedMessage,
    kept: &[Kept],
) -> EmbedBuilder {
    let card = message_card(
        ctx,
        label(ctx, title),
        color,
        saved.author.get(),
        saved.channel_id.get(),
        &saved.content,
        &saved.attachments,
    );
    if kept.is_empty() {
        card
    } else {
        card.field(label(ctx, "logs-files"), names(ctx, kept), false)
    }
}

/// A message with its author, channel, text and attachments.
fn message_card(
    ctx: &EventContext,
    title: String,
    color: Color,
    author: u64,
    channel: u64,
    text: &str,
    attachments: &[closure_core::event::AttachmentInfo],
) -> EmbedBuilder {
    let content = if text.is_empty() {
        label(ctx, "logs-no-text")
    } else {
        clip(text, FIELD_LIMIT)
    };
    let card = card(title, color)
        .field(
            label(ctx, "logs-author"),
            format!("<@{author}> ({author})"),
            true,
        )
        .field(label(ctx, "logs-channel"), format!("<#{channel}>"), true)
        .field(label(ctx, "logs-content"), content, false);
    if attachments.is_empty() {
        return card;
    }
    let listed: Vec<String> = attachments
        .iter()
        .map(|file| file.filename.clone())
        .collect();
    card.field(
        label(ctx, "logs-attachments"),
        clip(&listed.join("\n"), FIELD_LIMIT),
        false,
    )
}

/// A deleted message the history did not keep: its channel and id, and no text.
fn unkept_card(ctx: &EventContext, channel: u64, message: u64) -> EmbedBuilder {
    card(label(ctx, "logs-deleted"), Color::Error)
        .field(label(ctx, "logs-channel"), format!("<#{channel}>"), true)
        .field(label(ctx, "logs-message"), message.to_string(), true)
        .field(
            label(ctx, "logs-content"),
            label(ctx, "logs-not-kept"),
            false,
        )
}

fn member_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    user: &UserInfo,
) -> Result<LogEntry, ModuleError> {
    let card = card(label(ctx, title), color).field(
        label(ctx, "logs-member"),
        format!("<@{}> ({})", user.id.get(), user.name),
        false,
    );
    Ok(LogEntry {
        embed: build(card)?,
        files: Vec::new(),
    })
}

/// The names of files, one to a line; a file that was not kept says so.
fn names(ctx: &EventContext, files: &[Kept]) -> String {
    let lines: Vec<String> = files
        .iter()
        .map(|file| match file.bytes {
            Some(_) => file.filename.clone(),
            None => format!("{} ({})", file.filename, label(ctx, "logs-not-kept")),
        })
        .collect();
    clip(&lines.join("\n"), FIELD_LIMIT)
}

/// Cuts `text` to `limit` characters; the last one then marks that the text went on.
fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(limit - 1).collect();
    clipped.push('…');
    clipped
}

fn label(ctx: &EventContext, key: &str) -> String {
    ctx.t_in(ctx.locale, key, &Args::new())
}

fn card(title: String, color: Color) -> EmbedBuilder {
    EmbedBuilder::new().title(title).color(color)
}

fn build(card: EmbedBuilder) -> Result<Embed, ModuleError> {
    card.build()
        .map_err(|error| ModuleError::other(error.to_string()))
}

#[cfg(test)]
mod tests;
