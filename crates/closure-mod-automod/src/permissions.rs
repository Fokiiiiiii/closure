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
    if let Some(mut saved) = store.get::<Saved>(&key).await.map_err(failure)? {
        let until = until.max(saved.until);
        schedule(ctx, &key, until).await?;
        saved.until = until;
        store.set(&key, &saved).await.map_err(failure)?;
        return Ok(());
    }
    let Outcome::Channel(channel_info) = ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await?
    else {
        return Err(failure("unexpected outcome for get_channel"));
    };
    let original = channel_info
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
        });
    let saved = Saved {
        channel,
        target,
        original: original.clone(),
        until,
    };
    schedule(ctx, &key, until).await?;
    store.set(&key, &saved).await.map_err(failure)?;
    let mut overwrite = original.unwrap_or(OverwriteSpec {
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
    let action = match saved.original {
        Some(overwrite) => Action::SetPermissionOverwrite {
            channel_id: saved.channel,
            overwrite,
            reason,
        },
        None => Action::DeletePermissionOverwrite {
            channel_id: saved.channel,
            target: saved.target,
            reason,
        },
    };
    match ctx.api.execute(action).await {
        Ok(_) | Err(DiscordError::NotFound) => {
            store.delete(&key).await.map_err(failure)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
