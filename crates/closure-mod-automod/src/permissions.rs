use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use closure_core::action::{Action, Outcome, OverwriteSpec, OverwriteTarget, Reason};
use closure_core::error::DiscordError;
use closure_sdk::{ChannelId, EventContext, GuildId, JobCx, ModuleError};
use serde::{Deserialize, Serialize};
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

pub const RESTORE_JOB: &str = "automod_restore_permissions";
pub const NAMES: [&str; 12] = [
    "send_messages",
    "send_messages_in_threads",
    "add_reactions",
    "attach_files",
    "embed_links",
    "create_public_threads",
    "create_private_threads",
    "speak",
    "connect",
    "use_external_emojis",
    "mention_everyone",
    "send_tts_messages",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    SendMessages,
    SendMessagesInThreads,
    AddReactions,
    AttachFiles,
    EmbedLinks,
    CreatePublicThreads,
    CreatePrivateThreads,
    Speak,
    Connect,
    UseExternalEmojis,
    MentionEveryone,
    SendTtsMessages,
}

impl Permission {
    fn bit(self) -> Permissions {
        match self {
            Self::SendMessages => Permissions::SEND_MESSAGES,
            Self::SendMessagesInThreads => Permissions::SEND_MESSAGES_IN_THREADS,
            Self::AddReactions => Permissions::ADD_REACTIONS,
            Self::AttachFiles => Permissions::ATTACH_FILES,
            Self::EmbedLinks => Permissions::EMBED_LINKS,
            Self::CreatePublicThreads => Permissions::CREATE_PUBLIC_THREADS,
            Self::CreatePrivateThreads => Permissions::CREATE_PRIVATE_THREADS,
            Self::Speak => Permissions::SPEAK,
            Self::Connect => Permissions::CONNECT,
            Self::UseExternalEmojis => Permissions::USE_EXTERNAL_EMOJIS,
            Self::MentionEveryone => Permissions::MENTION_EVERYONE,
            Self::SendTtsMessages => Permissions::SEND_TTS_MESSAGES,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    channel: ChannelId,
    target: OverwriteTarget,
    original: Option<OverwriteSpec>,
    until: i64,
    /// Whether the change was made on Discord. A change that was saved and not made (it failed) is
    /// made at the next try, not only given more time.
    #[serde(default = "yes")]
    applied: bool,
    /// The permission bits this module changed, so that putting back touches those only. Entries
    /// from before this was kept stand for every bit the module can change.
    #[serde(default = "all_bits")]
    bits: u64,
}

const fn yes() -> bool {
    true
}

fn all_bits() -> u64 {
    [
        Permission::SendMessages,
        Permission::SendMessagesInThreads,
        Permission::AddReactions,
        Permission::AttachFiles,
        Permission::EmbedLinks,
        Permission::CreatePublicThreads,
        Permission::CreatePrivateThreads,
        Permission::Speak,
        Permission::Connect,
        Permission::UseExternalEmojis,
        Permission::MentionEveryone,
        Permission::SendTtsMessages,
    ]
    .iter()
    .fold(Permissions::empty(), |all, permission| {
        all | permission.bit()
    })
    .bits()
}

fn lock(guild: GuildId) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<GuildId, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        locks
            .entry(guild)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

fn key(channel: ChannelId, target: OverwriteTarget) -> String {
    let (kind, id) = match target {
        OverwriteTarget::Role(id) => ("role", id.get()),
        OverwriteTarget::Member(id) => ("member", id.get()),
    };
    format!("permissions:{}:{kind}:{id}", channel.get())
}

fn failure(error: impl std::fmt::Display) -> ModuleError {
    ModuleError::other(error.to_string())
}

async fn schedule(ctx: &EventContext, key: &str, until: i64) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(RESTORE_JOB, &key)
        .map_err(failure)?
        .key(key)
        .at(until)
        .await
        .map_err(failure)
}

async fn target(ctx: &EventContext, id: Option<&str>) -> Result<OverwriteTarget, ModuleError> {
    let id = match id {
        Some(id) => id.parse::<u64>().map_err(failure)?,
        None => ctx.guild_id.get(),
    };
    if id == ctx.guild_id.get() {
        return Ok(OverwriteTarget::Role(Id::new(id)));
    }
    let Outcome::Roles(roles) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await?
    else {
        return Err(failure("unexpected outcome for list_roles"));
    };
    if let Some(role) = roles.iter().find(|role| role.id.get() == id) {
        let bot = ctx
            .bot()
            .ok_or_else(|| failure("bot identity is unavailable"))?;
        let member = ctx.api.get_member(ctx.guild_id, bot).await?;
        let top = roles
            .iter()
            .filter(|role| member.roles.contains(&role.id))
            .map(|role| role.position)
            .max()
            .ok_or_else(|| failure("bot role is unavailable"))?;
        if role.position >= top {
            return Err(failure("target role is not below the bot's highest role"));
        }
        Ok(OverwriteTarget::Role(role.id))
    } else {
        let user = Id::new(id);
        if ctx.bot() == Some(user) {
            return Err(failure("the bot cannot target itself"));
        }
        ctx.api.get_member(ctx.guild_id, user).await?;
        Ok(OverwriteTarget::Member(user))
    }
}

/// The overwrite of `target` in the channel as it is now, when it has one.
async fn current_overwrite(
    ctx: &EventContext,
    channel: ChannelId,
    target: OverwriteTarget,
) -> Result<Option<OverwriteSpec>, ModuleError> {
    let Outcome::Channel(channel_info) = ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await?
    else {
        return Err(failure("unexpected outcome for get_channel"));
    };
    Ok(channel_info
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| failure("channel overwrites are unavailable"))?
        .iter()
        .find(|overwrite| match target {
            OverwriteTarget::Role(id) => {
                overwrite.kind == PermissionOverwriteType::Role && overwrite.id.get() == id.get()
            }
            OverwriteTarget::Member(id) => {
                overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == id.get()
            }
        })
        .map(|overwrite| OverwriteSpec {
            target,
            allow: overwrite.allow,
            deny: overwrite.deny,
        }))
}

#[allow(
    clippy::too_many_lines,
    reason = "one flow: find the overwrite, save the change, make it, mark it made"
)]
pub async fn change(
    ctx: &EventContext,
    channel: ChannelId,
    id: Option<&str>,
    changes: (&[Permission], &[Permission], &[Permission]),
    duration: Duration,
    reason: &str,
) -> Result<(), ModuleError> {
    let (deny, allow, clear) = changes;
    let gate = lock(ctx.guild_id);
    let _guard = gate.lock().await;
    let target = target(ctx, id).await?;
    let key = key(channel, target);
    let store = ctx.store();
    let until = ctx.now() + i64::try_from(duration.as_secs()).map_err(failure)?;
    let existing = store.get::<Saved>(&key).await.map_err(failure)?;
    if let Some(saved) = existing.as_ref().filter(|saved| saved.applied) {
        // Made already: only more time is given.
        let until = until.max(saved.until);
        schedule(ctx, &key, until).await?;
        let extended = Saved {
            until,
            channel: saved.channel,
            target: saved.target,
            original: saved.original.clone(),
            applied: true,
            bits: saved.bits,
        };
        store.set(&key, &extended).await.map_err(failure)?;
        return Ok(());
    }
    // What the channel has now is what the change is made on: an overwrite changed by somebody
    // since an earlier try keeps its other bits. A change that is new starts from it; one that
    // was saved and not made keeps the overwrite it started from, for the restore.
    let current = current_overwrite(ctx, channel, target).await?;
    let original = if let Some(saved) = &existing {
        saved.original.clone()
    } else {
        current.clone()
    };
    let touched = clear
        .iter()
        .chain(deny)
        .chain(allow)
        .fold(Permissions::empty(), |bits, permission| {
            bits | permission.bit()
        });
    let mut saved = Saved {
        channel,
        target,
        original: original.clone(),
        until: existing
            .as_ref()
            .map_or(until, |saved| until.max(saved.until)),
        applied: false,
        bits: touched.bits() | existing.as_ref().map_or(0, |saved| saved.bits),
    };
    schedule(ctx, &key, saved.until).await?;
    store.set(&key, &saved).await.map_err(failure)?;
    let mut overwrite = current.unwrap_or(OverwriteSpec {
        target,
        allow: Permissions::empty(),
        deny: Permissions::empty(),
    });
    for permission in clear.iter().chain(deny).chain(allow) {
        overwrite.allow.remove(permission.bit());
        overwrite.deny.remove(permission.bit());
    }
    for permission in deny {
        overwrite.deny.insert(permission.bit());
    }
    for permission in allow {
        overwrite.allow.insert(permission.bit());
    }
    ctx.api
        .execute(Action::SetPermissionOverwrite {
            channel_id: channel,
            overwrite,
            reason: Reason::truncated(reason),
        })
        .await?;
    saved.applied = true;
    store.set(&key, &saved).await.map_err(failure)?;
    Ok(())
}

pub async fn restore(cx: JobCx) -> Result<(), ModuleError> {
    let key: String = cx.payload()?;
    let ctx = cx.ctx();
    let gate = lock(ctx.guild_id);
    let _guard = gate.lock().await;
    let store = ctx.store();
    let Some(saved) = store.get::<Saved>(&key).await.map_err(failure)? else {
        return Ok(());
    };
    if saved.until > ctx.now() {
        return schedule(ctx, &key, saved.until).await;
    }
    if let OverwriteTarget::Role(role) = saved.target
        && role.get() != ctx.guild_id.get()
    {
        match ctx
            .api
            .execute(Action::ListRoles {
                guild_id: ctx.guild_id,
            })
            .await?
        {
            Outcome::Roles(roles) if !roles.iter().any(|item| item.id == role) => {
                store.delete(&key).await.map_err(failure)?;
                return Ok(());
            }
            Outcome::Roles(_) => {}
            _ => return Err(failure("unexpected outcome for list_roles")),
        }
        target(ctx, Some(&role.get().to_string())).await?;
    }
    let reason = Reason::truncated(&ctx.t("permission-restore-reason"));
    // Only the bits this module changed are put back: what somebody set by hand since is left.
    let mask = Permissions::from_bits_truncate(saved.bits);
    let current = match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: saved.channel,
        })
        .await
    {
        Ok(Outcome::Channel(info)) => info.permission_overwrites.as_deref().and_then(|list| {
            list.iter()
                .find(|item| match saved.target {
                    OverwriteTarget::Role(id) => {
                        item.kind == PermissionOverwriteType::Role && item.id.get() == id.get()
                    }
                    OverwriteTarget::Member(id) => {
                        item.kind == PermissionOverwriteType::Member && item.id.get() == id.get()
                    }
                })
                .map(|item| (item.allow, item.deny))
        }),
        Ok(_) => None,
        Err(DiscordError::NotFound) => {
            store.delete(&key).await.map_err(failure)?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let (original_allow, original_deny) = saved
        .original
        .as_ref()
        .map_or((Permissions::empty(), Permissions::empty()), |original| {
            (original.allow, original.deny)
        });
    let (allow, deny) = match current {
        Some((allow, deny)) => (
            (allow - mask) | (original_allow & mask),
            (deny - mask) | (original_deny & mask),
        ),
        // Nothing there now: the overwrite goes back as it was.
        None => (original_allow, original_deny),
    };
    let action = if allow.is_empty() && deny.is_empty() {
        Action::DeletePermissionOverwrite {
            channel_id: saved.channel,
            target: saved.target,
            reason,
        }
    } else {
        Action::SetPermissionOverwrite {
            channel_id: saved.channel,
            overwrite: OverwriteSpec {
                target: saved.target,
                allow,
                deny,
            },
            reason,
        }
    };
    match ctx.api.execute(action).await {
        Ok(_) | Err(DiscordError::NotFound) => {
            store.delete(&key).await.map_err(failure)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
