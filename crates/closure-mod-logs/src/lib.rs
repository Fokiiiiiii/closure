//! The logs module for closure: what happens to the messages and members of a server, posted to a
//! channel. A deleted message is shown with what the history kept of it: its text, and its files when
//! the host kept them. The files and their posting are the SDK's (`closure_sdk::files`).

use closure_core::capability::Capability;
use closure_core::error::{CoreError, ModuleError};
use closure_core::event::{
    AttachmentInfo, ChannelId, ChannelInfo, Event, EventKind, MemberInfo, MessageId, MessageInfo,
    RoleId, RoleInfo, RolesChange, StageInfo, StickerInfo, ThreadInfo, UserInfo,
};
use closure_core::history::SavedMessage;
use closure_core::locale::Locale;
use closure_core::module::EventContext;
use closure_core::subscription::{Flow, Subscription};
use closure_sdk::files::{self, Kept, LogEntry};
use closure_sdk::{
    Args, Color, EmbedBuilder, Intents, ModuleBuilder, SdkModule, TimeStyle, duration,
    parse_module_config,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::channel::message::Embed;
use twilight_model::id::Id;

mod audit;
use audit::{add_audit, audit_of, bulk_audit_of, deletion_audit_of};

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
    channels: bool,
    threads: bool,
    roles: bool,
    member_roles: bool,
    member_nicknames: bool,
    member_timeouts: bool,
    voice: bool,
    emojis: bool,
    stickers: bool,
    stage_instances: bool,
}

fn settings() -> (serde_json::Value, serde_json::Value) {
    let schema = json!({
        "type": "object",
        "properties": {
            "channel": {
                "anyOf": [{ "type": "string", "pattern": "^[1-9][0-9]*$" }, { "type": "null" }],
                "description": "config-logs-channel",
                "x-ui": { "level": "basic" }
            },
            "message_edits": { "type": "boolean", "description": "config-logs-message_edits" },
            "message_deletes": {
                "type": "boolean",
                "description": "config-logs-message_deletes",
                "x-ui": { "level": "basic" }
            },
            "member_joins": { "type": "boolean", "description": "config-logs-member_joins" },
            "member_leaves": { "type": "boolean", "description": "config-logs-member_leaves" },
            "bans": { "type": "boolean", "description": "config-logs-bans" },
            "channels": { "type": "boolean" },
            "threads": { "type": "boolean" },
            "roles": { "type": "boolean" },
            "member_roles": { "type": "boolean" },
            "member_nicknames": { "type": "boolean" },
            "member_timeouts": { "type": "boolean" },
            "voice": { "type": "boolean" },
            "emojis": { "type": "boolean" },
            "stickers": { "type": "boolean" },
            "stage_instances": { "type": "boolean" },
            "keep_attachments": {
                "type": "boolean",
                "description": "config-logs-keep_attachments",
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
        "channels": true,
        "threads": true,
        "roles": true,
        "member_roles": true,
        "member_nicknames": true,
        "member_timeouts": true,
        "voice": false,
        "emojis": true,
        "stickers": true,
        "stage_instances": true,
        "keep_attachments": false
    });
    (schema, defaults)
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = settings();
    Ok(
        ModuleBuilder::new("logs", "Logs", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::SendMessages)
            .capability(Capability::ReadMessageHistory)
            .capability(Capability::KeepAttachments)
            .capability(Capability::ReadAuditLog)
            .intents(
                Intents::GUILD_MESSAGES
                    | Intents::GUILD_MEMBERS
                    | Intents::GUILD_MODERATION
                    | Intents::MESSAGE_CONTENT
                    | Intents::GUILD_VOICE_STATES
                    | Intents::GUILD_EMOJIS_AND_STICKERS,
            )
            .on(Subscription::new(EventKind::MessageUpdated), on_event)
            .on(Subscription::new(EventKind::MessageDeleted), on_event)
            .on(Subscription::new(EventKind::MessagesBulkDeleted), on_event)
            .on(Subscription::new(EventKind::MemberJoined), on_event)
            .on(Subscription::new(EventKind::MemberLeft), on_event)
            .on(Subscription::new(EventKind::BanAdded), on_event)
            .on(Subscription::new(EventKind::BanRemoved), on_event)
            .on(Subscription::new(EventKind::ChannelCreated), on_event)
            .on(Subscription::new(EventKind::ChannelChanged), on_event)
            .on(Subscription::new(EventKind::ChannelDeleted), on_event)
            .on(Subscription::new(EventKind::ThreadCreated), on_event)
            .on(Subscription::new(EventKind::ThreadArchived), on_event)
            .on(Subscription::new(EventKind::ThreadUnarchived), on_event)
            .on(Subscription::new(EventKind::ThreadDeleted), on_event)
            .on(Subscription::new(EventKind::RoleCreated), on_event)
            .on(Subscription::new(EventKind::RoleChanged), on_event)
            .on(Subscription::new(EventKind::RoleDeleted), on_event)
            .on(Subscription::new(EventKind::RolesChanged), on_event)
            .on(Subscription::new(EventKind::NicknameChanged), on_event)
            .on(Subscription::new(EventKind::TimeoutChanged), on_event)
            .on(Subscription::new(EventKind::VoiceStateChanged), on_event)
            .on(Subscription::new(EventKind::EmojiCreated), on_event)
            .on(Subscription::new(EventKind::EmojiDeleted), on_event)
            .on(Subscription::new(EventKind::EmojiUpdated), on_event)
            .on(Subscription::new(EventKind::StickerCreated), on_event)
            .on(Subscription::new(EventKind::StickerDeleted), on_event)
            .on(Subscription::new(EventKind::StickerUpdated), on_event)
            .on(Subscription::new(EventKind::StageCreated), on_event)
            .on(Subscription::new(EventKind::StageDeleted), on_event)
            .on(Subscription::new(EventKind::StageUpdated), on_event)
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
    let Some(channel) = destination(&ctx, &config)? else {
        return Ok(Flow::Continue);
    };
    let entries = match &event {
        Event::MessageUpdated(message) if config.message_edits && edit_is_logged(message) => {
            vec![edited(&ctx, message).await?]
        }
        Event::MessageDeleted {
            channel_id,
            message_id,
            ..
        } if config.message_deletes => {
            let audit = deletion_audit_of(&ctx, *channel_id, *message_id).await;
            vec![add_audit(
                &ctx,
                deleted(&ctx, *channel_id, *message_id).await?,
                audit.as_ref(),
            )]
        }
        Event::MessagesBulkDeleted {
            channel_id,
            message_ids,
            ..
        } if config.message_deletes => {
            let audit = bulk_audit_of(&ctx, *channel_id, message_ids.len()).await;
            bulk_deleted(&ctx, *channel_id, message_ids)
                .await?
                .into_iter()
                .map(|entry| add_audit(&ctx, entry, audit.as_ref()))
                .collect()
        }
        Event::MemberJoined(member) if config.member_joins => {
            vec![joined_entry(&ctx, member)?]
        }
        Event::MemberLeft { user, .. } if config.member_leaves => {
            let kick = audit_of(&ctx, &config, &event).await;
            let (title, color) = if kick.is_some() {
                ("logs-kicked", Color::Error)
            } else {
                ("logs-left", Color::Info)
            };
            vec![add_audit(
                &ctx,
                member_entry(&ctx, title, color, user)?,
                kick.as_ref(),
            )]
        }
        Event::BanAdded { user, .. } if config.bans => {
            let ban = audit_of(&ctx, &config, &event).await;
            vec![add_audit(
                &ctx,
                member_entry(&ctx, "logs-banned", Color::Error, user)?,
                ban.as_ref(),
            )]
        }
        Event::BanRemoved { user, .. } if config.bans => {
            let unban = audit_of(&ctx, &config, &event).await;
            vec![add_audit(
                &ctx,
                member_entry(&ctx, "logs-unbanned", Color::Success, user)?,
                unban.as_ref(),
            )]
        }
        _ => match change_entries(&ctx, &config, &event)? {
            Some(entries) => {
                let audit = if entries.is_empty() {
                    None
                } else {
                    audit_of(&ctx, &config, &event).await
                };
                entries
                    .into_iter()
                    .map(|entry| add_audit(&ctx, entry, audit.as_ref()))
                    .collect()
            }
            None => return Ok(Flow::Continue),
        },
    };
    files::post(&ctx, channel, entries).await?;
    Ok(Flow::Continue)
}

/// The entries of the events that describe a change of a channel, a thread or a role. `None` for any
/// other event.
fn structure_entries(
    ctx: &EventContext,
    config: &Config,
    event: &Event,
) -> Result<Option<Vec<LogEntry>>, ModuleError> {
    let entries = match event {
        Event::ChannelCreated(channel) if config.channels => {
            vec![channel_entry(
                ctx,
                "logs-channel-created",
                Color::Success,
                channel,
            )?]
        }
        Event::ChannelChanged { before, after } if config.channels => {
            changed(channel_changed_entry(ctx, before, after)?)
        }
        Event::ChannelDeleted(channel) if config.channels => {
            vec![channel_entry(
                ctx,
                "logs-channel-deleted",
                Color::Error,
                channel,
            )?]
        }
        Event::ThreadCreated(thread) if config.threads => {
            vec![thread_entry(
                ctx,
                "logs-thread-created",
                Color::Success,
                thread,
            )?]
        }
        Event::ThreadArchived(thread) if config.threads => {
            vec![thread_entry(
                ctx,
                "logs-thread-archived",
                Color::Info,
                thread,
            )?]
        }
        Event::ThreadUnarchived(thread) if config.threads => {
            vec![thread_entry(
                ctx,
                "logs-thread-unarchived",
                Color::Info,
                thread,
            )?]
        }
        Event::ThreadDeleted {
            thread_id,
            parent_id,
            ..
        } if config.threads => vec![thread_deleted_entry(ctx, *thread_id, *parent_id)?],
        Event::RoleCreated(role) if config.roles => {
            vec![role_entry(ctx, "logs-role-created", Color::Success, role)?]
        }
        Event::RoleChanged { before, after } if config.roles => {
            changed(role_changed_entry(ctx, before, after)?)
        }
        Event::RoleDeleted { role_id, .. } if config.roles => {
            vec![role_deleted_entry(ctx, *role_id)?]
        }
        _ => return Ok(None),
    };
    Ok(Some(entries))
}

/// The entries of the events that describe a change of a member, a voice channel, an emoji, a sticker
/// or a stage. `None` for any other event.
fn state_entries(
    ctx: &EventContext,
    config: &Config,
    event: &Event,
) -> Result<Option<Vec<LogEntry>>, ModuleError> {
    let entries = match event {
        Event::RolesChanged(change) if config.member_roles => {
            vec![roles_changed_entry(ctx, change)?]
        }
        Event::NicknameChanged {
            user,
            before,
            after,
            ..
        } if config.member_nicknames => vec![nickname_entry(
            ctx,
            user,
            before.as_deref(),
            after.as_deref(),
        )?],
        Event::TimeoutChanged { user, after, .. } if config.member_timeouts => {
            vec![timeout_entry(ctx, user, *after)?]
        }
        Event::VoiceStateChanged {
            user,
            before,
            after,
            ..
        } if config.voice => vec![voice_entry(ctx, user, *before, *after)?],
        Event::EmojiCreated(emoji) if config.emojis => {
            vec![emoji_entry(
                ctx,
                "logs-emoji-created",
                Color::Success,
                &emoji.name,
            )?]
        }
        Event::EmojiDeleted(emoji) if config.emojis => {
            vec![emoji_entry(
                ctx,
                "logs-emoji-deleted",
                Color::Error,
                &emoji.name,
            )?]
        }
        Event::EmojiUpdated { before, after } if config.emojis => {
            changed(emoji_changed_entry(ctx, &before.name, &after.name)?)
        }
        Event::StickerCreated(sticker) if config.stickers => {
            vec![sticker_entry(
                ctx,
                "logs-sticker-created",
                Color::Success,
                sticker,
            )?]
        }
        Event::StickerDeleted(sticker) if config.stickers => {
            vec![sticker_entry(
                ctx,
                "logs-sticker-deleted",
                Color::Error,
                sticker,
            )?]
        }
        Event::StickerUpdated { before, after } if config.stickers => {
            changed(sticker_changed_entry(ctx, before, after)?)
        }
        Event::StageCreated(stage) if config.stage_instances => {
            vec![stage_entry(
                ctx,
                "logs-stage-started",
                Color::Success,
                stage,
            )?]
        }
        Event::StageDeleted(stage) if config.stage_instances => {
            vec![stage_entry(ctx, "logs-stage-ended", Color::Error, stage)?]
        }
        Event::StageUpdated { before, after } if config.stage_instances => {
            changed(stage_changed_entry(ctx, before, after)?)
        }
        _ => return Ok(None),
    };
    Ok(Some(entries))
}

/// The entries that a gateway event gives beyond the ones of messages and members, if it describes a
/// change that a log shows.
fn change_entries(
    ctx: &EventContext,
    config: &Config,
    event: &Event,
) -> Result<Option<Vec<LogEntry>>, ModuleError> {
    match structure_entries(ctx, config, event)? {
        Some(entries) => Ok(Some(entries)),
        None => state_entries(ctx, config, event),
    }
}

/// The channel the entries go to: the one the module is configured with, else the guild's log channel.
/// A configured value that is not a channel is an error, so the entries never go to another channel.
fn destination(ctx: &EventContext, config: &Config) -> Result<Option<ChannelId>, ModuleError> {
    let Some(text) = config.channel.as_deref() else {
        return Ok(ctx.policy().log_channel);
    };
    text.parse::<u64>()
        .ok()
        .and_then(Id::new_checked)
        .map(Some)
        .ok_or_else(|| ModuleError::Config(format!("`{text}` is not a channel id")))
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
    )
    .timestamp(ctx.now());
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

/// Whether an edit is worth an entry. Not the bot's own messages: an entry is one of them, and Discord edits
/// a message again when it unfurls the links in it, which would post an entry for every entry. Not an edit
/// that left the text as it was either.
fn edit_is_logged(message: &MessageInfo) -> bool {
    !message.author.bot && message.previous_content.as_deref() != Some(message.content.as_str())
}

/// An edit: the message, its files, the files the edit took away (`removed_attachments`), and the
/// text before the edit and after it.
async fn edited(ctx: &EventContext, message: &MessageInfo) -> Result<LogEntry, ModuleError> {
    let removed =
        files::files_of_attachments(ctx, message.id, &message.removed_attachments).await?;
    let card = message_card(
        ctx,
        label(ctx, "logs-edited"),
        Color::Warning,
        message.author.id.get(),
        message.channel_id.get(),
        message.id.get(),
        message.created_at,
    );
    let card = attachments_field(ctx, card, &message.attachments);
    let card = if removed.is_empty() {
        card
    } else {
        card.field(label(ctx, "logs-removed"), names(ctx, &removed), false)
    };
    let card = match &message.previous_content {
        Some(before) => text_field(ctx, card, "logs-before", before),
        None => card.field(
            label(ctx, "logs-before"),
            label(ctx, "logs-not-kept"),
            false,
        ),
    };
    let card = text_field(ctx, card, "logs-after", &message.content);
    Ok(LogEntry {
        embed: build(card)?,
        files: removed.iter().filter_map(Kept::upload).collect(),
    })
}

/// A saved message with its author, channel, id, the files that were kept of it, and its text.
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
        saved.id.get(),
        saved.created_at,
    );
    let card = attachments_field(ctx, card, &saved.attachments);
    let card = if kept.is_empty() {
        card
    } else {
        card.field(label(ctx, "logs-files"), names(ctx, kept), false)
    };
    text_field(ctx, card, "logs-content", &saved.content)
}

/// A message's author, channel and id, and when it was sent. The card carries the time of the
/// change it shows, as its timestamp.
fn message_card(
    ctx: &EventContext,
    title: String,
    color: Color,
    author: u64,
    channel: u64,
    message: u64,
    sent: i64,
) -> EmbedBuilder {
    card(title, color)
        .field(
            label(ctx, "logs-author"),
            format!("<@{author}> ({author})"),
            true,
        )
        .field(label(ctx, "logs-channel"), format!("<#{channel}>"), true)
        .field(label(ctx, "logs-message"), message.to_string(), true)
        .field(
            label(ctx, "logs-posted"),
            duration::timestamp(sent, TimeStyle::ShortDateTime),
            true,
        )
        .timestamp(ctx.now())
}

/// A text as a field. An empty one says that there is no text.
fn text_field(ctx: &EventContext, card: EmbedBuilder, key: &str, text: &str) -> EmbedBuilder {
    let value = if text.is_empty() {
        label(ctx, "logs-no-text")
    } else {
        code_block(text)
    };
    card.field(label(ctx, key), value, false)
}

/// The names of the attachments as a field, when there are any.
fn attachments_field(
    ctx: &EventContext,
    card: EmbedBuilder,
    attachments: &[AttachmentInfo],
) -> EmbedBuilder {
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
        .timestamp(ctx.now())
}

/// A join: the member, when the account was made and how old it is, and when the member joined.
fn joined_entry(ctx: &EventContext, member: &MemberInfo) -> Result<LogEntry, ModuleError> {
    let now = ctx.now();
    let created = duration::snowflake_time(member.user.id.get());
    let card = member_card(ctx, "logs-joined", Color::Success, &member.user)
        .field(
            label(ctx, "logs-created"),
            duration::timestamp(created, TimeStyle::ShortDateTime),
            true,
        )
        .field(
            label(ctx, "logs-age"),
            duration::timestamp(created, TimeStyle::Relative),
            true,
        )
        .field(
            label(ctx, "logs-joined-at"),
            duration::timestamp(member.joined_at.unwrap_or(now), TimeStyle::ShortDateTime),
            true,
        );
    Ok(LogEntry {
        embed: build(card)?,
        files: Vec::new(),
    })
}

fn member_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    user: &UserInfo,
) -> Result<LogEntry, ModuleError> {
    Ok(LogEntry {
        embed: build(member_card(ctx, title, color, user))?,
        files: Vec::new(),
    })
}

/// A card about a member: its title, and the member it is about.
fn member_card(ctx: &EventContext, title: &str, color: Color, user: &UserInfo) -> EmbedBuilder {
    card(label(ctx, title), color).field(
        label(ctx, "logs-member"),
        format!("<@{}> ({})", user.id.get(), user.name),
        false,
    )
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

/// The entries that a change gives: none when the change shows nothing.
fn changed(entry: Option<LogEntry>) -> Vec<LogEntry> {
    entry.into_iter().collect()
}

/// An entry without files.
fn plain(card: EmbedBuilder) -> Result<LogEntry, ModuleError> {
    Ok(LogEntry {
        embed: build(card)?,
        files: Vec::new(),
    })
}

/// A name in a code span; backticks in the name are kept apart from the span.
fn name_text(ctx: &EventContext, name: Option<&str>) -> String {
    match name {
        Some(name) if !name.is_empty() => format!("`{}`", name.replace('`', "'")),
        _ => label(ctx, "logs-none"),
    }
}

/// A text as a code block, or the word for none.
fn text_or_none(ctx: &EventContext, text: Option<&str>) -> String {
    match text {
        Some(text) if !text.is_empty() => code_block(text),
        _ => label(ctx, "logs-none"),
    }
}

fn channel_mention(ctx: &EventContext, id: Option<ChannelId>) -> String {
    id.map_or_else(|| label(ctx, "logs-none"), |id| format!("<#{}>", id.get()))
}

fn yes_or_no(ctx: &EventContext, value: bool) -> String {
    label(ctx, if value { "logs-yes" } else { "logs-no" })
}

fn arrow(before: &str, after: &str) -> String {
    format!("{before} → {after}")
}

fn role_mentions(roles: &[RoleId]) -> String {
    roles
        .iter()
        .map(|role| format!("<@&{}>", role.get()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn colour_text(colour: u32) -> String {
    format!("#{colour:06X}")
}

/// A text that changed: the text before it, then the text after it, each as a code block.
fn text_change(
    ctx: &EventContext,
    card: EmbedBuilder,
    key: &str,
    before: Option<&str>,
    after: Option<&str>,
) -> EmbedBuilder {
    card.field(label(ctx, key), text_or_none(ctx, before), false)
        .field(label(ctx, "logs-after"), text_or_none(ctx, after), false)
}

fn channel_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    channel: &ChannelInfo,
) -> Result<LogEntry, ModuleError> {
    let card = card(label(ctx, title), color)
        .field(
            label(ctx, "logs-channel"),
            format!("<#{}>", channel.id.get()),
            true,
        )
        .field(
            label(ctx, "logs-name"),
            name_text(ctx, channel.name.as_deref()),
            true,
        );
    let card = match channel.parent_id {
        Some(parent) => card.field(
            label(ctx, "logs-parent"),
            format!("<#{}>", parent.get()),
            true,
        ),
        None => card,
    };
    plain(card)
}

fn channel_changed_entry(
    ctx: &EventContext,
    before: &ChannelInfo,
    after: &ChannelInfo,
) -> Result<Option<LogEntry>, ModuleError> {
    let mut card = card(label(ctx, "logs-channel-changed"), Color::Warning).field(
        label(ctx, "logs-channel"),
        format!("<#{}>", after.id.get()),
        true,
    );
    let mut any = false;
    if before.name != after.name {
        let shown = arrow(
            &name_text(ctx, before.name.as_deref()),
            &name_text(ctx, after.name.as_deref()),
        );
        card = card.field(label(ctx, "logs-name"), shown, false);
        any = true;
    }
    if before.parent_id != after.parent_id {
        let shown = arrow(
            &channel_mention(ctx, before.parent_id),
            &channel_mention(ctx, after.parent_id),
        );
        card = card.field(label(ctx, "logs-parent"), shown, false);
        any = true;
    }
    if before.topic != after.topic {
        card = text_change(
            ctx,
            card,
            "logs-topic",
            before.topic.as_deref(),
            after.topic.as_deref(),
        );
        any = true;
    }
    if before.nsfw != after.nsfw {
        let shown = arrow(&yes_or_no(ctx, before.nsfw), &yes_or_no(ctx, after.nsfw));
        card = card.field(label(ctx, "logs-nsfw"), shown, false);
        any = true;
    }
    if any { plain(card).map(Some) } else { Ok(None) }
}

fn thread_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    thread: &ThreadInfo,
) -> Result<LogEntry, ModuleError> {
    plain(
        card(label(ctx, title), color)
            .field(
                label(ctx, "logs-thread"),
                format!("<#{}>", thread.id.get()),
                true,
            )
            .field(
                label(ctx, "logs-name"),
                name_text(ctx, Some(&thread.name)),
                true,
            )
            .field(
                label(ctx, "logs-parent"),
                channel_mention(ctx, thread.parent_id),
                true,
            ),
    )
}

fn thread_deleted_entry(
    ctx: &EventContext,
    thread: ChannelId,
    parent: ChannelId,
) -> Result<LogEntry, ModuleError> {
    plain(
        card(label(ctx, "logs-thread-deleted"), Color::Error)
            .field(
                label(ctx, "logs-thread"),
                format!("<#{}>", thread.get()),
                true,
            )
            .field(
                label(ctx, "logs-parent"),
                format!("<#{}>", parent.get()),
                true,
            ),
    )
}

fn role_card(ctx: &EventContext, title: &str, color: Color, role: &RoleInfo) -> EmbedBuilder {
    card(label(ctx, title), color)
        .field(
            label(ctx, "logs-role"),
            format!("<@&{}>", role.id.get()),
            true,
        )
        .field(
            label(ctx, "logs-name"),
            name_text(ctx, Some(&role.name)),
            true,
        )
}

fn role_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    role: &RoleInfo,
) -> Result<LogEntry, ModuleError> {
    plain(role_card(ctx, title, color, role))
}

fn role_deleted_entry(ctx: &EventContext, role: RoleId) -> Result<LogEntry, ModuleError> {
    plain(card(label(ctx, "logs-role-deleted"), Color::Error).field(
        label(ctx, "logs-role"),
        format!("<@&{}>", role.get()),
        true,
    ))
}

fn role_changed_entry(
    ctx: &EventContext,
    before: &RoleInfo,
    after: &RoleInfo,
) -> Result<Option<LogEntry>, ModuleError> {
    let mut card = role_card(ctx, "logs-role-changed", Color::Warning, after);
    let mut any = false;
    if before.name != after.name {
        let shown = arrow(
            &name_text(ctx, Some(&before.name)),
            &name_text(ctx, Some(&after.name)),
        );
        card = card.field(label(ctx, "logs-name"), shown, false);
        any = true;
    }
    if before.color != after.color {
        let shown = arrow(&colour_text(before.color), &colour_text(after.color));
        card = card.field(label(ctx, "logs-colour"), shown, false);
        any = true;
    }
    if before.position != after.position {
        let shown = arrow(&before.position.to_string(), &after.position.to_string());
        card = card.field(label(ctx, "logs-position"), shown, false);
        any = true;
    }
    if before.hoist != after.hoist {
        let shown = arrow(&yes_or_no(ctx, before.hoist), &yes_or_no(ctx, after.hoist));
        card = card.field(label(ctx, "logs-hoist"), shown, false);
        any = true;
    }
    if before.mentionable != after.mentionable {
        let shown = arrow(
            &yes_or_no(ctx, before.mentionable),
            &yes_or_no(ctx, after.mentionable),
        );
        card = card.field(label(ctx, "logs-mentionable"), shown, false);
        any = true;
    }
    if any { plain(card).map(Some) } else { Ok(None) }
}

fn roles_changed_entry(ctx: &EventContext, change: &RolesChange) -> Result<LogEntry, ModuleError> {
    let mut card = member_card(ctx, "logs-member-roles-changed", Color::Info, &change.user);
    if !change.added.is_empty() {
        let shown = role_mentions(&change.added);
        card = card.field(label(ctx, "logs-roles-added"), shown, false);
    }
    if !change.removed.is_empty() {
        let shown = role_mentions(&change.removed);
        card = card.field(label(ctx, "logs-roles-removed"), shown, false);
    }
    plain(card)
}

fn nickname_entry(
    ctx: &EventContext,
    user: &UserInfo,
    before: Option<&str>,
    after: Option<&str>,
) -> Result<LogEntry, ModuleError> {
    let card = member_card(ctx, "logs-nickname-changed", Color::Info, user)
        .field(label(ctx, "logs-before"), text_or_none(ctx, before), false)
        .field(label(ctx, "logs-after"), text_or_none(ctx, after), false);
    plain(card)
}

fn timeout_entry(
    ctx: &EventContext,
    user: &UserInfo,
    until: Option<i64>,
) -> Result<LogEntry, ModuleError> {
    let mut card = match until {
        Some(_) => member_card(ctx, "logs-timeout-set", Color::Warning, user),
        None => member_card(ctx, "logs-timeout-lifted", Color::Success, user),
    };
    if let Some(until) = until {
        let shown = duration::timestamp(until, TimeStyle::ShortDateTime);
        card = card.field(label(ctx, "logs-until"), shown, true);
    }
    plain(card)
}

fn voice_entry(
    ctx: &EventContext,
    user: &UserInfo,
    before: Option<ChannelId>,
    after: Option<ChannelId>,
) -> Result<LogEntry, ModuleError> {
    let title = match (before, after) {
        (None, Some(_)) => "logs-voice-joined",
        (Some(_), None) => "logs-voice-left",
        _ => "logs-voice-moved",
    };
    let mut card = member_card(ctx, title, Color::Info, user);
    if let Some(from) = before {
        card = card.field(label(ctx, "logs-from"), format!("<#{}>", from.get()), true);
    }
    if let Some(to) = after {
        card = card.field(label(ctx, "logs-to"), format!("<#{}>", to.get()), true);
    }
    plain(card)
}

fn emoji_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    name: &str,
) -> Result<LogEntry, ModuleError> {
    plain(card(label(ctx, title), color).field(label(ctx, "logs-emoji"), format!(":{name}:"), true))
}

fn emoji_changed_entry(
    ctx: &EventContext,
    before: &str,
    after: &str,
) -> Result<Option<LogEntry>, ModuleError> {
    if before == after {
        return Ok(None);
    }
    let shown = arrow(&format!(":{before}:"), &format!(":{after}:"));
    plain(
        card(label(ctx, "logs-emoji-changed"), Color::Warning).field(
            label(ctx, "logs-emoji"),
            shown,
            false,
        ),
    )
    .map(Some)
}

fn sticker_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    sticker: &StickerInfo,
) -> Result<LogEntry, ModuleError> {
    let card = card(label(ctx, title), color).field(
        label(ctx, "logs-name"),
        name_text(ctx, Some(&sticker.name)),
        true,
    );
    let card = if sticker.description.is_empty() {
        card
    } else {
        card.field(
            label(ctx, "logs-description"),
            code_block(&sticker.description),
            false,
        )
    };
    plain(card)
}

fn sticker_changed_entry(
    ctx: &EventContext,
    before: &StickerInfo,
    after: &StickerInfo,
) -> Result<Option<LogEntry>, ModuleError> {
    let mut card = card(label(ctx, "logs-sticker-changed"), Color::Warning);
    let mut any = false;
    if before.name != after.name {
        let shown = arrow(
            &name_text(ctx, Some(&before.name)),
            &name_text(ctx, Some(&after.name)),
        );
        card = card.field(label(ctx, "logs-name"), shown, false);
        any = true;
    }
    if before.description != after.description {
        card = text_change(
            ctx,
            card,
            "logs-description",
            Some(&before.description),
            Some(&after.description),
        );
        any = true;
    }
    if any { plain(card).map(Some) } else { Ok(None) }
}

fn stage_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    stage: &StageInfo,
) -> Result<LogEntry, ModuleError> {
    plain(
        card(label(ctx, title), color)
            .field(
                label(ctx, "logs-channel"),
                format!("<#{}>", stage.channel_id.get()),
                true,
            )
            .field(
                label(ctx, "logs-topic"),
                text_or_none(ctx, Some(&stage.topic)),
                false,
            ),
    )
}

fn stage_changed_entry(
    ctx: &EventContext,
    before: &StageInfo,
    after: &StageInfo,
) -> Result<Option<LogEntry>, ModuleError> {
    if before.topic == after.topic {
        return Ok(None);
    }
    let card = card(label(ctx, "logs-stage-changed"), Color::Warning).field(
        label(ctx, "logs-channel"),
        format!("<#{}>", after.channel_id.get()),
        true,
    );
    let card = text_change(
        ctx,
        card,
        "logs-topic",
        Some(&before.topic),
        Some(&after.topic),
    );
    plain(card).map(Some)
}

/// Text as a code block, so that it reads as written and nothing in it formats the entry. A run of
/// backticks is broken with a zero-width space, so that the text cannot end the block early.
fn code_block(text: &str) -> String {
    let escaped = text.replace("```", "`\u{200B}``");
    format!("```{}```", clip(&escaped, FIELD_LIMIT - CODE_FENCE_CHARS))
}

/// The characters that the code block around a text adds to it.
const CODE_FENCE_CHARS: usize = 6;

#[cfg(test)]
mod tests;
