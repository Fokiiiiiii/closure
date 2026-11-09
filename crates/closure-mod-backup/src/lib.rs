//! A backup of the roles and channels of a server, and a way to bring back what is gone.
//!
//! `/backup create` keeps the roles and channels; a daily run keeps them again unless half of them are gone.
//! `/backup restore` recreates what is missing; existing items are left alone, so it is safe to run twice.
//!
//! Not kept: role and channel order, member roles, member overwrites, threads, webhooks, messages.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use closure_core::action::{
    Action, ChannelKind, NewChannel, NewRole, Outcome, OverwriteSpec, OverwriteTarget, Reason,
};
use closure_core::event::{ChannelId, Event, EventKind, RoleId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, Capability, Command, CommandCx, CoreError, JobCx, Level, Locale, ModuleBuilder,
    ModuleError, PermissionSpec, SdkModule, Subscription, UserError, parse_module_config,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
use twilight_model::channel::{Channel, ChannelType};
use twilight_model::guild::{Permissions, Role};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const DAILY: &str = "daily";
const META: &str = "meta";
/// A backup that would hold less than this share of the last one is not taken by itself.
const SHRINK_LIMIT_PERCENT: usize = 50;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    daily: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { daily: true }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    at: i64,
    roles: Vec<u64>,
    channels: Vec<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedRole {
    id: u64,
    name: String,
    color: u32,
    permissions: u64,
    hoist: bool,
    mentionable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedOverwrite {
    role: u64,
    allow: u64,
    deny: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedChannel {
    id: u64,
    name: String,
    kind: ChannelKind,
    parent: Option<u64>,
    topic: Option<String>,
    nsfw: bool,
    slowmode: u16,
    overwrites: Vec<SavedOverwrite>,
}

fn stored(error: &closure_core::storage::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn role_key(id: u64) -> String {
    format!("role:{id}")
}

fn channel_key(id: u64) -> String {
    format!("channel:{id}")
}

fn number(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

fn kind_of(kind: ChannelType) -> Option<ChannelKind> {
    match kind {
        ChannelType::GuildText => Some(ChannelKind::Text),
        ChannelType::GuildVoice => Some(ChannelKind::Voice),
        ChannelType::GuildCategory => Some(ChannelKind::Category),
        ChannelType::GuildAnnouncement => Some(ChannelKind::Announcement),
        ChannelType::GuildForum => Some(ChannelKind::Forum),
        ChannelType::GuildStageVoice => Some(ChannelKind::Stage),
        _ => None,
    }
}

fn saved_role(role: &Role) -> SavedRole {
    SavedRole {
        id: role.id.get(),
        name: role.name.clone(),
        color: role.colors.primary_color,
        permissions: role.permissions.bits(),
        hoist: role.hoist,
        mentionable: role.mentionable,
    }
}

fn saved_channel(channel: &Channel) -> Option<SavedChannel> {
    let kind = kind_of(channel.kind)?;
    let overwrites = channel
        .permission_overwrites
        .iter()
        .flatten()
        .filter(|overwrite| overwrite.kind == PermissionOverwriteType::Role)
        .map(|overwrite| SavedOverwrite {
            role: overwrite.id.get(),
            allow: overwrite.allow.bits(),
            deny: overwrite.deny.bits(),
        })
        .collect();
    Some(SavedChannel {
        id: channel.id.get(),
        name: channel.name.clone()?,
        kind,
        parent: channel.parent_id.map(twilight_model::id::Id::get),
        topic: channel.topic.clone(),
        nsfw: channel.nsfw.unwrap_or(false),
        slowmode: channel.rate_limit_per_user.unwrap_or(0),
        overwrites,
    })
}

async fn current(ctx: &EventContext) -> Result<(Vec<Role>, Vec<Channel>), ModuleError> {
    let Outcome::Roles(roles) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await?
    else {
        return Err(ModuleError::other(
            "the roles of the server are unavailable",
        ));
    };
    let Outcome::Channels(channels) = ctx
        .api
        .execute(Action::ListChannels {
            guild_id: ctx.guild_id,
        })
        .await?
    else {
        return Err(ModuleError::other(
            "the channels of the server are unavailable",
        ));
    };
    Ok((roles, channels))
}

async fn last(ctx: &EventContext) -> Result<Option<Meta>, ModuleError> {
    ctx.store().get(META).await.map_err(|e| stored(&e))
}

/// Keeps the roles and channels of the server. `None` when `force` is off and the server holds
/// less than half of what the last backup does, which is left as it is.
async fn take(ctx: &EventContext, force: bool) -> Result<Option<Meta>, ModuleError> {
    let (roles, channels) = current(ctx).await?;
    // @everyone has the id of the server and always exists; a managed role belongs to a bot.
    let roles: Vec<SavedRole> = roles
        .iter()
        .filter(|role| !role.managed && role.id.get() != ctx.guild_id.get())
        .map(saved_role)
        .collect();
    let channels: Vec<SavedChannel> = channels.iter().filter_map(saved_channel).collect();
    let before = last(ctx).await?;
    if !force && let Some(before) = &before {
        let shrank = |now: usize, then: usize| now * 100 < then * SHRINK_LIMIT_PERCENT;
        if shrank(roles.len(), before.roles.len()) || shrank(channels.len(), before.channels.len())
        {
            tracing::warn!(
                guild = ctx.guild_id.get(),
                "the server shrank by half: the backup is kept as it was"
            );
            return Ok(None);
        }
    }
    let store = ctx.store();
    for role in &roles {
        store
            .set(&role_key(role.id), role)
            .await
            .map_err(|e| stored(&e))?;
    }
    for channel in &channels {
        store
            .set(&channel_key(channel.id), channel)
            .await
            .map_err(|e| stored(&e))?;
    }
    let meta = Meta {
        at: ctx.now(),
        roles: roles.iter().map(|role| role.id).collect(),
        channels: channels.iter().map(|channel| channel.id).collect(),
    };
    store.set(META, &meta).await.map_err(|e| stored(&e))?;
    // What the earlier backup held and this one does not is forgotten.
    if let Some(before) = before {
        for id in before.roles.iter().filter(|id| !meta.roles.contains(id)) {
            store.delete(&role_key(*id)).await.map_err(|e| stored(&e))?;
        }
        for id in before
            .channels
            .iter()
            .filter(|id| !meta.channels.contains(id))
        {
            store
                .delete(&channel_key(*id))
                .await
                .map_err(|e| stored(&e))?;
        }
    }
    Ok(Some(meta))
}

#[derive(Debug, Default)]
struct Restored {
    roles: usize,
    channels: usize,
    failed: usize,
    /// What was made, by the id the role or channel had in the backup.
    made_roles: HashMap<u64, u64>,
    made_channels: HashMap<u64, u64>,
}

/// Creates the roles of the backup the server lacks; the id each role of the backup has now.
async fn restore_roles(
    ctx: &EventContext,
    meta: &Meta,
    roles: &[Role],
    reason: &str,
    done: &mut Restored,
) -> Result<HashMap<u64, u64>, ModuleError> {
    let have_roles: HashSet<u64> = roles.iter().map(|role| role.id.get()).collect();
    let store = ctx.store();
    // The id each role of the backup has now: its own when it exists, else the new one.
    let mut now_role: HashMap<u64, u64> = have_roles.iter().map(|id| (*id, *id)).collect();
    for id in meta.roles.iter().filter(|id| !have_roles.contains(id)) {
        let Some(saved) = store
            .get::<SavedRole>(&role_key(*id))
            .await
            .map_err(|e| stored(&e))?
        else {
            continue;
        };
        let created = ctx
            .api
            .execute(Action::CreateRole {
                guild_id: ctx.guild_id,
                spec: NewRole {
                    name: saved.name,
                    color: saved.color,
                    permissions: Permissions::from_bits_truncate(saved.permissions),
                    hoist: saved.hoist,
                    mentionable: saved.mentionable,
                },
                reason: Reason::truncated(reason),
            })
            .await;
        match created {
            Ok(Outcome::Role(role)) => {
                now_role.insert(*id, role.id.get());
                done.made_roles.insert(*id, role.id.get());
                done.roles += 1;
            }
            Ok(_) => done.failed += 1,
            Err(error) => {
                tracing::warn!(%error, "could not create a role again");
                done.failed += 1;
            }
        }
    }
    Ok(now_role)
}

async fn restore_channels(
    ctx: &EventContext,
    meta: &Meta,
    channels: &[Channel],
    now_role: &HashMap<u64, u64>,
    reason: &str,
    done: &mut Restored,
) -> Result<(), ModuleError> {
    let have_channels: HashSet<u64> = channels.iter().map(|channel| channel.id.get()).collect();
    let store = ctx.store();
    let mut missing = Vec::new();
    for id in meta
        .channels
        .iter()
        .filter(|id| !have_channels.contains(id))
    {
        if let Some(saved) = store
            .get::<SavedChannel>(&channel_key(*id))
            .await
            .map_err(|e| stored(&e))?
        {
            missing.push(saved);
        }
    }
    // Categories first, so that what they hold can be put in them.
    missing.sort_by_key(|channel| channel.kind != ChannelKind::Category);
    let mut now_channel: HashMap<u64, u64> = have_channels.iter().map(|id| (*id, *id)).collect();
    for saved in missing {
        // A channel whose category is in the backup but could not be made waits for the next
        // restore, so that it is not made at the top and left there.
        if saved.parent.is_some_and(|parent| {
            meta.channels.contains(&parent) && !now_channel.contains_key(&parent)
        }) {
            done.failed += 1;
            continue;
        }
        let overwrites = saved
            .overwrites
            .iter()
            .filter_map(|overwrite| {
                let role = if overwrite.role == ctx.guild_id.get() {
                    overwrite.role
                } else {
                    *now_role.get(&overwrite.role)?
                };
                Some(OverwriteSpec {
                    target: OverwriteTarget::Role(RoleId::new_checked(role)?),
                    allow: Permissions::from_bits_truncate(overwrite.allow),
                    deny: Permissions::from_bits_truncate(overwrite.deny),
                })
            })
            .collect();
        let parent_id = saved
            .parent
            .and_then(|parent| now_channel.get(&parent))
            .and_then(|parent| ChannelId::new_checked(*parent));
        let created = ctx
            .api
            .execute(Action::CreateChannel {
                guild_id: ctx.guild_id,
                spec: NewChannel {
                    name: saved.name,
                    kind: saved.kind,
                    parent_id,
                    topic: saved.topic,
                    nsfw: saved.nsfw,
                    slowmode_seconds: saved.slowmode,
                    overwrites,
                },
                reason: Reason::truncated(reason),
            })
            .await;
        match created {
            Ok(Outcome::Channel(channel)) => {
                now_channel.insert(saved.id, channel.id.get());
                done.made_channels.insert(saved.id, channel.id.get());
                done.channels += 1;
            }
            Ok(_) => done.failed += 1,
            Err(error) => {
                tracing::warn!(%error, "could not create a channel again");
                done.failed += 1;
            }
        }
    }
    Ok(())
}

/// Names what a restore made in the backup, so that a second restore finds it there. What could
/// not be made keeps its record under its old id, to be made at the next restore. The new records
/// are written first and the old ones go last, so that a stop at any point leaves a backup that
/// is whole.
async fn rebase(ctx: &EventContext, meta: &Meta, done: &Restored) -> Result<(), ModuleError> {
    let store = ctx.store();
    let swap = |id: u64, made: &HashMap<u64, u64>| made.get(&id).copied().unwrap_or(id);
    let mut stale = Vec::new();
    for id in &meta.roles {
        let Some(mut saved) = store
            .get::<SavedRole>(&role_key(*id))
            .await
            .map_err(|e| stored(&e))?
        else {
            continue;
        };
        let now = swap(*id, &done.made_roles);
        if now != *id {
            saved.id = now;
            store
                .set(&role_key(now), &saved)
                .await
                .map_err(|e| stored(&e))?;
            stale.push(role_key(*id));
        }
    }
    for id in &meta.channels {
        let Some(mut saved) = store
            .get::<SavedChannel>(&channel_key(*id))
            .await
            .map_err(|e| stored(&e))?
        else {
            continue;
        };
        let now = swap(*id, &done.made_channels);
        saved.id = now;
        saved.parent = saved.parent.map(|parent| swap(parent, &done.made_channels));
        for overwrite in &mut saved.overwrites {
            overwrite.role = swap(overwrite.role, &done.made_roles);
        }
        store
            .set(&channel_key(now), &saved)
            .await
            .map_err(|e| stored(&e))?;
        if now != *id {
            stale.push(channel_key(*id));
        }
    }
    let next = Meta {
        at: meta.at,
        roles: meta
            .roles
            .iter()
            .map(|id| swap(*id, &done.made_roles))
            .collect(),
        channels: meta
            .channels
            .iter()
            .map(|id| swap(*id, &done.made_channels))
            .collect(),
    };
    store.set(META, &next).await.map_err(|e| stored(&e))?;
    for key in stale {
        store.delete(&key).await.map_err(|e| stored(&e))?;
    }
    Ok(())
}

async fn restore(ctx: &EventContext, meta: &Meta, reason: &str) -> Result<Restored, ModuleError> {
    let (roles, channels) = current(ctx).await?;
    let mut done = Restored::default();
    let now_role = restore_roles(ctx, meta, &roles, reason, &mut done).await?;
    restore_channels(ctx, meta, &channels, &now_role, reason, &mut done).await?;
    Ok(done)
}

fn failure(error: &ModuleError) -> UserError {
    tracing::warn!(%error, "backup failed");
    UserError::message("backup-failed")
}

async fn create(cx: CommandCx, (): ()) -> Result<(), UserError> {
    cx.defer(true).await?;
    let meta = take(cx.ctx(), true)
        .await
        .map_err(|error| failure(&error))?
        .unwrap_or_default();
    cx.reply_private_t(
        "backup-created",
        &Args::new()
            .with("roles", number(meta.roles.len()))
            .with("channels", number(meta.channels.len())),
    )
    .await
}

async fn info(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let Some(meta) = last(cx.ctx()).await.map_err(|error| failure(&error))? else {
        return cx.reply_private_t("backup-none", &Args::new()).await;
    };
    cx.reply_private_t(
        "backup-info",
        &Args::new()
            .with("when", format!("<t:{}:f>", meta.at))
            .with("roles", number(meta.roles.len()))
            .with("channels", number(meta.channels.len())),
    )
    .await
}

async fn restore_command(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let Some(meta) = last(cx.ctx()).await.map_err(|error| failure(&error))? else {
        return cx.reply_private_t("backup-none", &Args::new()).await;
    };
    cx.defer(true).await?;
    let reason = cx.t("backup-reason", &Args::new());
    let done = restore(cx.ctx(), &meta, &reason)
        .await
        .map_err(|error| failure(&error))?;
    rebase(cx.ctx(), &meta, &done)
        .await
        .map_err(|error| failure(&error))?;
    cx.reply_private_t(
        "backup-restored",
        &Args::new()
            .with("roles", number(done.roles))
            .with("channels", number(done.channels))
            .with("failed", number(done.failed)),
    )
    .await
}

/// The server came up: the daily backup is set going, if it is not already.
async fn available(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    if !matches!(event, Event::GuildAvailable { .. }) {
        return Ok(Flow::Continue);
    }
    let config: Config = parse_module_config(&ctx)?;
    if config.daily {
        ctx.scheduler()
            .job(DAILY, &json!({}))
            .map_err(|error| ModuleError::other(error.to_string()))?
            .key(DAILY)
            .if_absent()
            .every(Duration::from_hours(24), Duration::from_mins(10))
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    Ok(Flow::Continue)
}

async fn daily(cx: JobCx) -> Result<(), ModuleError> {
    let ctx = cx.ctx();
    let config: Config = parse_module_config(ctx)?;
    if config.daily {
        take(ctx, false).await?;
    }
    Ok(())
}

/// The module before it is built, so that tests can run it on the SDK's pretend Discord.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "daily": {
                "type": "boolean",
                "description": "config-backup-daily",
                "x-ui": { "level": "basic" }
            }
        }
    });
    Ok(
        ModuleBuilder::new("backup", "Backup", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::ReadGuild)
            .capability(Capability::ManageRoles)
            .capability(Capability::ManageChannels)
            .capability(Capability::Storage)
            .capability(Capability::Schedule)
            .permission(PermissionSpec::new(
                "backup.manage",
                Level::ADMIN,
                "permission-backup-manage",
            )?)
            .config(schema, json!({ "daily": true }))
            .on(Subscription::new(EventKind::GuildAvailable), available)
            .job(DAILY, daily)
            .group(
                "backup",
                [
                    Command::<()>::new("create")
                        .requires("backup.manage")
                        .handler(create),
                    Command::<()>::new("info")
                        .requires("backup.manage")
                        .handler(info),
                    Command::<()>::new("restore")
                        .requires("backup.manage")
                        .handler(restore_command),
                ],
            )
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
