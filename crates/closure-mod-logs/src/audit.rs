//! Who made a change, from the audit log. Discord reports a kick as a leave, and the events of a change
//! do not name whoever made it; the audit log names both. An event is given the newest entry of its kind
//! that names the same target within [`AUDIT_WINDOW_SECONDS`]. Where values are available, they are
//! checked against the event; the time window alone cannot distinguish every repeated action.

use closure_core::action::{Action, AuditLogQuery, Outcome};
use closure_core::event::{ChannelId, Event, MessageId, UserId};
use closure_core::module::EventContext;
use closure_sdk::duration::snowflake_time;
use closure_sdk::files::LogEntry;
use twilight_model::channel::message::embed::EmbedField;
use twilight_model::guild::audit_log::{AuditLogChange, AuditLogEntry, AuditLogEventType};

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

impl AuditEntry {
    /// Whether the change was made by `user`, when it is known.
    pub(crate) fn made_by(&self, user: Option<UserId>) -> bool {
        self.by.is_some() && self.by == user
    }
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
        entry.target_id.is_some_and(|id| id.get() == target)
            && within_window(now, entry)
            && changes_the_field(entry, event)
    })?;
    Some(AuditEntry {
        // A member's own change to its nickname or timeout names no one.
        by: entry.user_id.filter(|by| by.get() != target),
        reason: entry.reason.clone(),
    })
}

/// Whether an entry's values match the event's change.
fn changes_the_field(entry: &AuditLogEntry, event: &Event) -> bool {
    match event {
        Event::NicknameChanged { before, after, .. } => entry.changes.iter().any(|change| {
            matches!(change, AuditLogChange::Nick { old, new } if old == before && new == after)
        }),
        Event::TimeoutChanged { before, after, .. } => entry.changes.iter().any(|change| {
            matches!(change, AuditLogChange::CommunicationDisabledUntil { old, new }
                if old.map(twilight_model::util::Timestamp::as_secs) == *before && new.map(twilight_model::util::Timestamp::as_secs) == *after)
        }),
        Event::ChannelChanged { before, after } => {
            permissions::overwrite_changes(&before.overwrites, &after.overwrites).is_empty()
                && [before.name != after.name, before.topic != after.topic, before.nsfw != after.nsfw, before.parent_id != after.parent_id].into_iter().filter(|changed| *changed).count() == 1
                && entry.changes.iter().any(|change| match change {
                    AuditLogChange::Name { old, new } => old == &before.name && new == &after.name && before.name != after.name,
                    AuditLogChange::Topic { old, new } => old == &before.topic && new == &after.topic && before.topic != after.topic,
                    AuditLogChange::Nsfw { old, new } => *old == Some(before.nsfw) && *new == Some(after.nsfw) && before.nsfw != after.nsfw,
                    _ => false,
                })
        }
        Event::RoleChanged { before, after } => {
            [before.name != after.name, before.color != after.color, before.position != after.position, before.hoist != after.hoist, before.mentionable != after.mentionable, before.permissions != after.permissions].into_iter().filter(|changed| *changed).count() == 1
                && entry.changes.iter().any(|change| match change {
                    AuditLogChange::Name { old, new } => old.as_ref() == Some(&before.name) && new.as_ref() == Some(&after.name) && before.name != after.name,
                    AuditLogChange::Color { old, new } => *old == Some(u64::from(before.color)) && *new == Some(u64::from(after.color)) && before.color != after.color,
                    AuditLogChange::Position { old, new } => old.and_then(|v| i64::try_from(v).ok()) == Some(before.position) && new.and_then(|v| i64::try_from(v).ok()) == Some(after.position) && before.position != after.position,
                    AuditLogChange::Hoist { old, new } => *old == Some(before.hoist) && *new == Some(after.hoist) && before.hoist != after.hoist,
                    AuditLogChange::Mentionable { old, new } => *old == Some(before.mentionable) && *new == Some(after.mentionable) && before.mentionable != after.mentionable,
                    AuditLogChange::Permissions { old, new } => *old == Some(before.permissions) && *new == Some(after.permissions) && before.permissions != after.permissions,
                    _ => false,
                })
        }
        Event::EmojiUpdated { before, after } => before.name != after.name && entry.changes.iter().any(|change| matches!(change, AuditLogChange::Name { old, new } if old.as_ref() == Some(&before.name) && new.as_ref() == Some(&after.name))),
        Event::StickerUpdated { before, after } => {
            (before.name != after.name) != (before.description != after.description)
                && entry.changes.iter().any(|change| match change {
                    AuditLogChange::Name { old, new } => before.name != after.name && old.as_ref() == Some(&before.name) && new.as_ref() == Some(&after.name),
                    AuditLogChange::Description { old, new } => before.description != after.description && old.as_ref() == Some(&before.description) && new.as_ref() == Some(&after.description),
                    _ => false,
                })
        }
        Event::StageUpdated { before, after } => before.topic != after.topic && entry.changes.iter().any(|change| matches!(change, AuditLogChange::Topic { old, new } if old.as_ref() == Some(&before.topic) && new.as_ref() == Some(&after.topic))),
        Event::ThreadArchived(_) => entry.changes.iter().any(|change| matches!(change, AuditLogChange::Archived { old: Some(false), new: Some(true) })),
        Event::ThreadUnarchived(_) => entry.changes.iter().any(|change| matches!(change, AuditLogChange::Archived { old: Some(true), new: Some(false) })),
        Event::RolesChanged(change) => {
            let mut added = Vec::new();
            let mut removed = Vec::new();
            for item in &entry.changes {
                match item {
                    AuditLogChange::RoleAdded { new, old } if old.is_empty() => added.extend(new.iter().map(|role| role.id)),
                    AuditLogChange::RoleRemoved { new, old } if old.is_empty() => removed.extend(new.iter().map(|role| role.id)),
                    _ => {}
                }
            }
            added.sort_unstable();
            removed.sort_unstable();
            let mut expected_added = change.added.clone();
            let mut expected_removed = change.removed.clone();
            expected_added.sort_unstable();
            expected_removed.sort_unstable();
            (!added.is_empty() || !removed.is_empty()) && added == expected_added && removed == expected_removed
        }
        _ => true,
    }
}

pub(crate) async fn change_audit_of(
    ctx: &EventContext,
    config: &Config,
    event: &Event,
) -> Option<AuditEntry> {
    if let Event::ChannelChanged { before, after } = event {
        let changes = permissions::overwrite_changes(&before.overwrites, &after.overwrites);
        if !changes.is_empty() {
            if changes.len() != 1
                || !config.channels
                || before.name != after.name
                || before.topic != after.topic
                || before.parent_id != after.parent_id
                || before.nsfw != after.nsfw
            {
                return None;
            }
            let change = &changes[0];
            let existed = before
                .overwrites
                .iter()
                .any(|item| item.target == change.target);
            let exists = after
                .overwrites
                .iter()
                .any(|item| item.target == change.target);
            let kind = match (existed, exists) {
                (false, true) => AuditLogEventType::ChannelOverwriteCreate,
                (true, false) => AuditLogEventType::ChannelOverwriteDelete,
                (true, true) => AuditLogEventType::ChannelOverwriteUpdate,
                (false, false) => return None,
            };
            return overwrite_audit_of(ctx, after.id, change, kind).await;
        }
    }
    audit_of(ctx, config, event).await
}

async fn overwrite_audit_of(
    ctx: &EventContext,
    channel: ChannelId,
    change: &permissions::OverwriteChange,
    kind: AuditLogEventType,
) -> Option<AuditEntry> {
    let entries = recent_entries(ctx, kind).await?;
    let entry = entries.iter().find(|entry| {
        let matches_permissions = |old: Option<twilight_model::guild::Permissions>, new: Option<twilight_model::guild::Permissions>, before, after| {
            match kind {
                AuditLogEventType::ChannelOverwriteCreate => old.is_none() && new == Some(after),
                AuditLogEventType::ChannelOverwriteDelete => old == Some(before) && new.is_none(),
                AuditLogEventType::ChannelOverwriteUpdate => old == Some(before) && new == Some(after),
                _ => false,
            }
        };
        let allow = change.before.allow == change.after.allow || entry.changes.iter().any(|item| {
            matches!(item, AuditLogChange::Allow { old, new } if matches_permissions(*old, *new, change.before.allow, change.after.allow))
        });
        let deny = change.before.deny == change.after.deny || entry.changes.iter().any(|item| {
            matches!(item, AuditLogChange::Deny { old, new } if matches_permissions(*old, *new, change.before.deny, change.after.deny))
        });
        let consistent = entry.changes.iter().all(|item| match item {
            AuditLogChange::Allow { old, new } => matches_permissions(*old, *new, change.before.allow, change.after.allow),
            AuditLogChange::Deny { old, new } => matches_permissions(*old, *new, change.before.deny, change.after.deny),
            _ => true,
        });
        entry.target_id.is_some_and(|id| id.get() == channel.get())
            && entry.options.as_ref().is_some_and(|options| options.id.is_some_and(|id| id.get() == permissions::target_id(change.target)))
            && within_window(ctx.now(), entry) && allow && deny && consistent
    })?;
    Some(AuditEntry {
        by: entry.user_id,
        reason: entry.reason.clone(),
    })
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
/// the channel and the number of messages in the entry, not the messages themselves. The documentation
/// does not promise the channel for a bulk delete, so an entry without it names no one.
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
