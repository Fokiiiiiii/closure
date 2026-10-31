//! Who made a change, from the audit log. Discord reports a kick as a leave, and the events of a change
//! do not name whoever made it; the audit log names both. An event is given the newest entry of its kind
//! that names the same target within [`AUDIT_WINDOW_SECONDS`], so an older change to the same member or
//! role is never named for this one.

use closure_core::action::{Action, AuditLogQuery, Outcome};
use closure_core::event::{Event, UserId};
use closure_core::module::EventContext;
use closure_sdk::duration::snowflake_time;
use closure_sdk::files::LogEntry;
use twilight_model::channel::message::embed::EmbedField;
use twilight_model::guild::audit_log::AuditLogEventType;

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
pub(crate) async fn audit_of(
    ctx: &EventContext,
    config: &Config,
    event: &Event,
) -> Option<AuditEntry> {
    let (kind, target) = audited_change(config, event)?;
    let query = AuditLogQuery {
        user_id: None,
        action_type: Some(u16::from(kind)),
        before: None,
        limit: Some(AUDIT_LOOKUP_LIMIT),
    };
    let log = match ctx
        .api
        .execute(Action::GetAuditLog {
            guild_id: ctx.guild_id,
            query,
        })
        .await
    {
        Ok(Outcome::AuditLog(log)) => log,
        Ok(outcome) => {
            tracing::warn!(?outcome, "the audit log did not answer with its entries");
            return None;
        }
        Err(error) => {
            tracing::warn!(
                guild = ctx.guild_id.get(),
                %error,
                "could not read the audit log; the entry is posted without who made the change"
            );
            return None;
        }
    };
    let now = ctx.now();
    let entry = log.entries.iter().find(|entry| {
        entry.target_id.is_some_and(|id| id.get() == target)
            && now - snowflake_time(entry.id.get()) <= AUDIT_WINDOW_SECONDS
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
