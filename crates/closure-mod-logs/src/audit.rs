//! Who made a change, from the audit log. Discord reports a kick as a leave, and the events of a change
//! do not name whoever made it; the audit log names both. An event is given the newest entry of its kind
//! that names the same target within [`AUDIT_WINDOW_SECONDS`], so an older change to the same member or
//! role is never named for this one.

use closure_core::action::{Action, AuditLogQuery, Outcome};
use closure_core::event::{ChannelId, Event, MessageId, UserId};
use closure_core::module::EventContext;
use closure_sdk::duration::snowflake_time;
use closure_sdk::files::LogEntry;
use twilight_model::channel::message::embed::EmbedField;
use twilight_model::guild::audit_log::{AuditLogEntry, AuditLogEventType};

use crate::permissions;
use crate::{Config, code_block, label};

/// How old an audit log entry may be and still be the change an event reports. Assumed, not measured: a
/// minute covers the gateway delivering the event after its change, and keeps an older change to the same
/// target from being named.
const AUDIT_WINDOW_SECONDS: i64 = 60;

/// How many of the newest entries of one kind are searched for the change.
const AUDIT_LOOKUP_LIMIT: u16 = 50;

/// The audit log entry of a change: who made it, and the reason they gave.
pub(crate) struct AuditEntry {
    by: Option<UserId>,
    reason: Option<String>,
}

/// The audit log action of the change an event reports, and the id it names as the target. `None` for an
/// event that is not a change of a named target, or whose kind the configuration leaves out.
fn audited_change(config: &Config, event: &Event) -> Option<(AuditLogEventType, u64)> {
    let change = match event {
        Event::MemberLeft { user, .. } if config.member_leaves => {
            (AuditLogEventType::MemberKick, user.id.get())
        }
        Event::BanAdded { user, .. } if config.bans => {
            (AuditLogEventType::MemberBanAdd, user.id.get())
        }
        Event::BanRemoved { user, .. } if config.bans => {
            (AuditLogEventType::MemberBanRemove, user.id.get())
        }
        Event::VoiceStateChanged {
            user,
            before: Some(_),
            after: None,
            ..
        } if config.voice => (AuditLogEventType::MemberDisconnect, user.id.get()),
        Event::VoiceStateChanged {
            user,
            before: Some(_),
            after: Some(_),
            ..
        } if config.voice => (AuditLogEventType::MemberMove, user.id.get()),
        Event::ChannelCreated(channel) if config.channels => {
            (AuditLogEventType::ChannelCreate, channel.id.get())
        }
        Event::ChannelChanged { after, .. } if config.channels => {
            (AuditLogEventType::ChannelUpdate, after.id.get())
        }
        Event::ChannelDeleted(channel) if config.channels => {
            (AuditLogEventType::ChannelDelete, channel.id.get())
        }
        Event::ThreadCreated(thread) if config.threads => {
            (AuditLogEventType::ThreadCreate, thread.id.get())
        }
        Event::ThreadArchived(thread) | Event::ThreadUnarchived(thread) if config.threads => {
            (AuditLogEventType::ThreadUpdate, thread.id.get())
        }
        Event::ThreadDeleted { thread_id, .. } if config.threads => {
            (AuditLogEventType::ThreadDelete, thread_id.get())
        }
        Event::RoleCreated(role) if config.roles => (AuditLogEventType::RoleCreate, role.id.get()),
        Event::RoleChanged { after, .. } if config.roles => {
            (AuditLogEventType::RoleUpdate, after.id.get())
        }
        Event::RoleDeleted { role_id, .. } if config.roles => {
            (AuditLogEventType::RoleDelete, role_id.get())
        }
        Event::RolesChanged(change) if config.member_roles => {
            (AuditLogEventType::MemberRoleUpdate, change.user.id.get())
        }
        Event::NicknameChanged { user, .. } if config.member_nicknames => {
            (AuditLogEventType::MemberUpdate, user.id.get())
        }
        Event::TimeoutChanged { user, .. } if config.member_timeouts => {
            (AuditLogEventType::MemberUpdate, user.id.get())
        }
        Event::EmojiCreated(emoji) if config.emojis => {
            (AuditLogEventType::EmojiCreate, emoji.id.get())
        }
        Event::EmojiUpdated { after, .. } if config.emojis => {
            (AuditLogEventType::EmojiUpdate, after.id.get())
        }
        Event::EmojiDeleted(emoji) if config.emojis => {
            (AuditLogEventType::EmojiDelete, emoji.id.get())
        }
        Event::StickerCreated(sticker) if config.stickers => {
            (AuditLogEventType::StickerCreate, sticker.id.get())
        }
        Event::StickerUpdated { after, .. } if config.stickers => {
            (AuditLogEventType::StickerUpdate, after.id.get())
        }
        Event::StickerDeleted(sticker) if config.stickers => {
            (AuditLogEventType::StickerDelete, sticker.id.get())
        }
        Event::StageCreated(stage) if config.stage_instances => {
            (AuditLogEventType::StageInstanceCreate, stage.id.get())
        }
        Event::StageUpdated { after, .. } if config.stage_instances => {
            (AuditLogEventType::StageInstanceUpdate, after.id.get())
        }
        Event::StageDeleted(stage) if config.stage_instances => {
            (AuditLogEventType::StageInstanceDelete, stage.id.get())
        }
        _ => return None,
    };
    Some(change)
}

/// The audit log entry of the change `event` reports, when there is one. `None` for an event without a
/// change to look up, when no entry of it is recent, or when the audit log cannot be read: the entry is
/// then posted without who made the change, and a failure to read is logged.
/// The newest entries of one kind, as Discord gives them. `None` when they cannot be read: the failure is
/// logged, and the entry is posted without who made the change.
async fn recent_entries(ctx: &EventContext, kind: AuditLogEventType) -> Option<Vec<AuditLogEntry>> {
    let query = AuditLogQuery {
        user_id: None,
        action_type: Some(u16::from(kind)),
        before: None,
        limit: Some(AUDIT_LOOKUP_LIMIT),
    };
    match ctx
        .api
        .execute(Action::GetAuditLog {
            guild_id: ctx.guild_id,
            query,
        })
        .await
    {
        Ok(Outcome::AuditLog(log)) => Some(log.entries),
        Ok(outcome) => {
            tracing::warn!(?outcome, "the audit log did not answer with its entries");
            None
        }
        Err(error) => {
            tracing::warn!(
                guild = ctx.guild_id.get(),
                %error,
                "could not read the audit log; the entry is posted without who made the change"
            );
            None
        }
    }
}

/// Whether an entry is recent enough to be the change an event reports.
fn within_window(now: i64, entry: &AuditLogEntry) -> bool {
    now - snowflake_time(entry.id.get()) <= AUDIT_WINDOW_SECONDS
}

/// The audit log entry of the change `event` reports, when there is one. `None` for an event without a
/// change to look up, when no entry of it is recent, or when the audit log cannot be read.
pub(crate) async fn audit_of(
    ctx: &EventContext,
    config: &Config,
    event: &Event,
) -> Option<AuditEntry> {
    let (kind, target) = audited_change(config, event)?;
    let entries = recent_entries(ctx, kind).await?;
    let now = ctx.now();
    let entry = entries.iter().find(|entry| {
        entry.target_id.is_some_and(|id| id.get() == target) && within_window(now, entry)
    })?;
    Some(AuditEntry {
        by: entry.user_id,
        reason: entry.reason.clone(),
    })
}

/// The audit log entry of a change: the change of the event's own target, or, for a channel whose
/// permission overwrites changed, the change of one of them.
pub(crate) async fn change_audit_of(
    ctx: &EventContext,
    config: &Config,
    event: &Event,
) -> Option<AuditEntry> {
    if let Some(audit) = audit_of(ctx, config, event).await {
        return Some(audit);
    }
    let Event::ChannelChanged { before, after } = event else {
        return None;
    };
    let targets: Vec<u64> = permissions::overwrite_changes(&before.overwrites, &after.overwrites)
        .iter()
        .map(|change| permissions::target_id(change.target))
        .collect();
    if targets.is_empty() {
        return None;
    }
    overwrite_audit_of(ctx, after.id, &targets).await
}

/// The audit log entry that changed a permission overwrite of `channel` for one of `targets`, when there
/// is one. Discord names the channel in the options of the entry, and the role or member in its target.
async fn overwrite_audit_of(
    ctx: &EventContext,
    channel: ChannelId,
    targets: &[u64],
) -> Option<AuditEntry> {
    let now = ctx.now();
    for kind in [
        AuditLogEventType::ChannelOverwriteCreate,
        AuditLogEventType::ChannelOverwriteUpdate,
        AuditLogEventType::ChannelOverwriteDelete,
    ] {
        let entries = recent_entries(ctx, kind).await?;
        let found = entries.iter().find(|entry| {
            entry
                .target_id
                .is_some_and(|id| targets.contains(&id.get()))
                && entry
                    .options
                    .as_ref()
                    .is_some_and(|options| options.channel_id == Some(channel))
                && within_window(now, entry)
        });
        if let Some(entry) = found {
            return Some(AuditEntry {
                by: entry.user_id,
                reason: entry.reason.clone(),
            });
        }
    }
    None
}

/// The audit log entry of a message that someone other than its author deleted. The entry names the author
/// and the channel of a deletion, not the message, so the author is read from the history: a message the
/// history does not hold names no one. A deletion by the author has no entry and names no one either.
pub(crate) async fn deletion_audit_of(
    ctx: &EventContext,
    channel: ChannelId,
    message: MessageId,
) -> Option<AuditEntry> {
    let author = match ctx.history().get(channel, message).await {
        Ok(saved) => saved?.author,
        Err(error) => {
            tracing::warn!(
                guild = ctx.guild_id.get(),
                %error,
                "could not read the message, so the deletion is posted without who made it"
            );
            return None;
        }
    };
    let entries = recent_entries(ctx, AuditLogEventType::MessageDelete).await?;
    let now = ctx.now();
    let entry = entries.iter().find(|entry| {
        entry.target_id.is_some_and(|id| id.get() == author.get())
            && entry
                .options
                .as_ref()
                .is_some_and(|options| options.channel_id == Some(channel))
            && within_window(now, entry)
    })?;
    let by = entry.user_id?;
    if by == author {
        return None;
    }
    Some(AuditEntry {
        by: Some(by),
        reason: entry.reason.clone(),
    })
}

/// The audit log entry of a bulk delete of `count` messages in `channel`, when there is one. Discord names
/// the channel and the number of messages in the entry, not the messages themselves.
pub(crate) async fn bulk_audit_of(
    ctx: &EventContext,
    channel: ChannelId,
    count: usize,
) -> Option<AuditEntry> {
    let entries = recent_entries(ctx, AuditLogEventType::MessageBulkDelete).await?;
    let now = ctx.now();
    let entry = entries.iter().find(|entry| {
        entry.options.as_ref().is_some_and(|options| {
            options.channel_id == Some(channel)
                && options
                    .count
                    .as_deref()
                    .and_then(|count| count.parse::<usize>().ok())
                    == Some(count)
        }) && within_window(now, entry)
    })?;
    Some(AuditEntry {
        by: entry.user_id,
        reason: entry.reason.clone(),
    })
}

/// The entry with who made its change, and the reason they gave, when the audit log named them.
pub(crate) fn add_audit(
    ctx: &EventContext,
    mut entry: LogEntry,
    audit: Option<&AuditEntry>,
) -> LogEntry {
    let Some(audit) = audit else {
        return entry;
    };
    if let Some(by) = audit.by {
        entry.embed.fields.push(EmbedField {
            inline: true,
            name: label(ctx, "logs-by"),
            value: format!("<@{}>", by.get()),
        });
    }
    if let Some(reason) = audit.reason.as_deref().filter(|reason| !reason.is_empty()) {
        entry.embed.fields.push(EmbedField {
            inline: false,
            name: label(ctx, "logs-reason"),
            value: code_block(reason),
        });
    }
    entry
}
