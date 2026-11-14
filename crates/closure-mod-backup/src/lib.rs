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
use closure_core::storage::{Expect, StorageError};
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
/// Held by whoever is saving or restoring, so that two of them never work on the backup at once.
const LOCK: &str = "lock";
const LOCK_FOR: Duration = Duration::from_mins(15);
/// What a restore has made so far, so that one that stopped is carried on, not made again.
const PROGRESS: &str = "progress";
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
    /// Which save the rows belong to; a save writes a new one and then points here, so a stop in
    /// the middle leaves the last whole backup as it was. 0 is a backup from before.
    #[serde(default)]
    generation: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Progress {
    roles: HashMap<u64, u64>,
    channels: HashMap<u64, u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedRole {
    id: u64,
    name: String,
    color: u32,
    /// The extra colours of a gradient or holographic role; a backup from before has none.
    #[serde(default)]
    secondary_color: Option<u32>,
    #[serde(default)]
    tertiary_color: Option<u32>,
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

fn role_key(generation: u64, id: u64) -> String {
    if generation == 0 {
        format!("role:{id}")
    } else {
        format!("g{generation}:role:{id}")
    }
}

fn channel_key(generation: u64, id: u64) -> String {
    if generation == 0 {
        format!("channel:{id}")
    } else {
        format!("g{generation}:channel:{id}")
    }
}

/// Takes the right to work on the backup; false when someone else has it.
async fn lock(ctx: &EventContext) -> Result<bool, ModuleError> {
    match ctx
        .store()
        .replace(LOCK, &ctx.now(), Expect::Missing, Some(LOCK_FOR))
        .await
    {
        Ok(_) => Ok(true),
        Err(StorageError::Conflict) => Ok(false),
        Err(error) => Err(stored(&error)),
    }
}

async fn unlock(ctx: &EventContext) {
    if let Err(error) = ctx.store().delete(LOCK).await {
        tracing::warn!(%error, "could not give the backup lock back");
    }
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
        secondary_color: role.colors.secondary_color,
        tertiary_color: role.colors.tertiary_color,
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

enum Taken {
    Kept(Meta),
    /// The server holds less than half of what the last backup does: it is left as it is.
    Shrank,
    /// A save or a restore is under way.
    Busy,
}

/// Keeps the roles and channels of the server (unless `force` is off and the server shrank).
async fn take(ctx: &EventContext, force: bool) -> Result<Taken, ModuleError> {
    if !lock(ctx).await? {
        return Ok(Taken::Busy);
    }
    let taken = save(ctx, force).await;
    unlock(ctx).await;
    taken
}

async fn save(ctx: &EventContext, force: bool) -> Result<Taken, ModuleError> {
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
            return Ok(Taken::Shrank);
        }
    }
    let store = ctx.store();
    let generation = before.as_ref().map_or(0, |before| before.generation) + 1;
    // Rows of a save that stopped before it was pointed to are not part of this one.
    let prefix = format!("g{generation}:");
    for (key, _) in store.list(&prefix, 1000).await.map_err(|e| stored(&e))? {
        store.delete(&key).await.map_err(|e| stored(&e))?;
    }
    for role in &roles {
        store
            .set(&role_key(generation, role.id), role)
            .await
            .map_err(|e| stored(&e))?;
    }
    for channel in &channels {
        store
            .set(&channel_key(generation, channel.id), channel)
            .await
            .map_err(|e| stored(&e))?;
    }
    let meta = Meta {
        at: ctx.now(),
        roles: roles.iter().map(|role| role.id).collect(),
        channels: channels.iter().map(|channel| channel.id).collect(),
        generation,
    };
    store.set(META, &meta).await.map_err(|e| stored(&e))?;
    // The save before this one is forgotten, now that nothing points to it.
    if let Some(before) = before {
        for id in &before.roles {
            store
                .delete(&role_key(before.generation, *id))
                .await
                .map_err(|e| stored(&e))?;
        }
        for id in &before.channels {
            store
                .delete(&channel_key(before.generation, *id))
                .await
                .map_err(|e| stored(&e))?;
        }
    }
    Ok(Taken::Kept(meta))
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
        // Made by a restore that stopped: it is there under its new id.
        if let Some(now) = done.made_roles.get(id) {
            now_role.insert(*id, *now);
            continue;
        }
        let Some(saved) = store
            .get::<SavedRole>(&role_key(meta.generation, *id))
            .await
            .map_err(|e| stored(&e))?
        else {
            // The backup names this role but its row is gone: counted, not skipped.
            done.failed += 1;
            continue;
        };
        let created = ctx
            .api
            .execute(Action::CreateRole {
                guild_id: ctx.guild_id,
                spec: NewRole {
                    name: saved.name,
                    color: saved.color,
                    secondary_color: saved.secondary_color,
                    tertiary_color: saved.tertiary_color,
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
                remember(ctx, done).await?;
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
    let mut now_channel: HashMap<u64, u64> = have_channels.iter().map(|id| (*id, *id)).collect();
    for id in meta
        .channels
        .iter()
        .filter(|id| !have_channels.contains(id))
    {
        if let Some(now) = done.made_channels.get(id) {
            now_channel.insert(*id, *now);
            continue;
        }
        let Some(saved) = store
            .get::<SavedChannel>(&channel_key(meta.generation, *id))
            .await
            .map_err(|e| stored(&e))?
        else {
            done.failed += 1;
            continue;
        };
        missing.push(saved);
    }
    // Categories first, so that what they hold can be put in them.
    missing.sort_by_key(|channel| channel.kind != ChannelKind::Category);
    for saved in missing {
        // A channel whose category is in the backup but could not be made waits for the next
        // restore, so that it is not made at the top and left there.
        if saved.parent.is_some_and(|parent| {
            meta.channels.contains(&parent) && !now_channel.contains_key(&parent)
        }) {
            done.failed += 1;
            continue;
        }
        // Made without its overwrite, the channel would keep it missing, so it waits.
        if saved.overwrites.iter().any(|overwrite| {
            meta.roles.contains(&overwrite.role) && !now_role.contains_key(&overwrite.role)
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
                remember(ctx, done).await?;
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
async fn rebase(
    ctx: &EventContext,
    meta: &Meta,
    revision: i64,
    done: &Restored,
) -> Result<(), ModuleError> {
    let store = ctx.store();
    let swap = |id: u64, made: &HashMap<u64, u64>| made.get(&id).copied().unwrap_or(id);
    let mut stale = Vec::new();
    for id in &meta.roles {
        let Some(mut saved) = store
            .get::<SavedRole>(&role_key(meta.generation, *id))
            .await
            .map_err(|e| stored(&e))?
        else {
            continue;
        };
        let now = swap(*id, &done.made_roles);
        if now != *id {
            saved.id = now;
            store
                .set(&role_key(meta.generation, now), &saved)
                .await
                .map_err(|e| stored(&e))?;
            stale.push(role_key(meta.generation, *id));
        }
    }
    for id in &meta.channels {
        let Some(mut saved) = store
            .get::<SavedChannel>(&channel_key(meta.generation, *id))
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
            .set(&channel_key(meta.generation, now), &saved)
            .await
            .map_err(|e| stored(&e))?;
        if now != *id {
            stale.push(channel_key(meta.generation, *id));
        }
    }
    let next = Meta {
        at: meta.at,
        generation: meta.generation,
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
    store
        .replace(META, &next, Expect::Revision(revision), None)
        .await
        .map_err(|e| stored(&e))?;
    for key in stale {
        store.delete(&key).await.map_err(|e| stored(&e))?;
    }
    store.delete(PROGRESS).await.map_err(|e| stored(&e))?;
    Ok(())
}

/// Writes down what the restore has made, so that a stop in the middle does not make it twice.
async fn remember(ctx: &EventContext, done: &Restored) -> Result<(), ModuleError> {
    let progress = Progress {
        roles: done.made_roles.clone(),
        channels: done.made_channels.clone(),
    };
    ctx.store()
        .set(PROGRESS, &progress)
        .await
        .map_err(|e| stored(&e))
}

async fn restore(ctx: &EventContext, meta: &Meta, reason: &str) -> Result<Restored, ModuleError> {
    let (roles, channels) = current(ctx).await?;
    let mut done = Restored::default();
    // What an earlier restore made counts when it is still there.
    if let Some(progress) = ctx
        .store()
        .get::<Progress>(PROGRESS)
        .await
        .map_err(|e| stored(&e))?
    {
        let (have_roles, have_channels): (HashSet<u64>, HashSet<u64>) = (
            roles.iter().map(|role| role.id.get()).collect(),
            channels.iter().map(|channel| channel.id.get()).collect(),
        );
        done.made_roles = progress
            .roles
            .into_iter()
            .filter(|(_, now)| have_roles.contains(now))
            .collect();
        done.made_channels = progress
            .channels
            .into_iter()
            .filter(|(_, now)| have_channels.contains(now))
            .collect();
    }
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
    let meta = match take(cx.ctx(), true)
        .await
        .map_err(|error| failure(&error))?
    {
        Taken::Kept(meta) => meta,
        Taken::Busy => return cx.reply_private_t("backup-busy", &Args::new()).await,
        Taken::Shrank => Meta::default(),
    };
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
    cx.defer(true).await?;
    if !lock(cx.ctx()).await.map_err(|error| failure(&error))? {
        return cx.reply_private_t("backup-busy", &Args::new()).await;
    }
    let reason = cx.t("backup-reason", &Args::new());
    let outcome = restore_under_lock(cx.ctx(), &reason).await;
    unlock(cx.ctx()).await;
    let Some(done) = outcome.map_err(|error| failure(&error))? else {
        return cx.reply_private_t("backup-none", &Args::new()).await;
    };
    cx.reply_private_t(
        "backup-restored",
        &Args::new()
            .with("roles", number(done.roles))
            .with("channels", number(done.channels))
            .with("failed", number(done.failed)),
    )
    .await
}

/// The lock is held, so no save can replace the backup between the read and the rebase.
async fn restore_under_lock(
    ctx: &EventContext,
    reason: &str,
) -> Result<Option<Restored>, ModuleError> {
    let Some(entry) = ctx.store().entry(META).await.map_err(|e| stored(&e))? else {
        return Ok(None);
    };
    let meta: Meta = serde_json::from_value(entry.value)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    #[cfg(test)]
    race::pause().await;
    let done = restore(ctx, &meta, reason).await?;
    rebase(ctx, &meta, entry.revision, &done).await?;
    Ok(Some(done))
}

/// The server came up, or its settings changed: the daily backup is set going, if it is not already.
async fn available(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    if !matches!(
        event,
        Event::GuildAvailable { .. } | Event::ConfigChanged { .. }
    ) {
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
            .on(Subscription::new(EventKind::ConfigChanged), available)
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

/// Lets a test run a save after the restore has read the backup; thread-local, one test per thread.
#[cfg(test)]
mod race {
    use std::cell::RefCell;
    use std::sync::Arc;

    use tokio::sync::Notify;

    thread_local! {
        pub(crate) static PAUSE: RefCell<Option<(Arc<Notify>, Arc<Notify>)>> = const { RefCell::new(None) };
    }

    pub(crate) async fn pause() {
        let hook = PAUSE.with(|pause| pause.borrow_mut().take());
        if let Some((reached, go)) = hook {
            reached.notify_one();
            go.notified().await;
        }
    }
}
