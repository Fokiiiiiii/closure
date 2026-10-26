//! The dedicated roles of the role method. A timeout or a voice mute that gives a role uses one
//! that Closure creates on first use, or finds by name, and sets up in every channel of the guild
//! so that the role takes the speech away there. An explicitly configured role is never touched.

use std::time::Duration;

use closure_core::action::{
    Action, MentionPolicy, NewRole, Outcome, OverwriteSpec, OverwriteTarget, Reason,
};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, RoleId, UserId};
use closure_core::locale::Args;
use closure_core::module::EventContext;
use closure_core::schedule::ScheduleError;
use closure_core::storage::StorageError;
use serde::{Deserialize, Serialize};
use twilight_model::channel::Channel;
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
use twilight_model::guild::{Permissions, Role};

use super::{RoleSelection, STRIP_LIMIT};
use crate::{JobCx, UserError};

/// The job that sets a dedicated role up in the channels, a batch of them per run.
const SYNC_JOB: &str = "restriction_sync";
/// The channels one run of the sync sets up.
const SYNC_BATCH: usize = 10;
/// The channels the sync looks at, lowest id first. shortcut: a guild with more channels than this
/// leaves the rest unchecked, which the log note says; upgrade by paging the channel list.
const SYNC_CHANNELS: usize = 500;

/// Which restriction a dedicated role stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestrictionRole {
    /// A timeout.
    Timeout,
    /// A voice mute.
    Voice,
}

impl RestrictionRole {
    /// The part of store keys and job keys that tells the two apart.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Voice => "vcmute",
        }
    }

    /// The name a created role has. Found by name without regard to case, so that two modules in
    /// one guild share one role.
    #[must_use]
    pub const fn role_name(self) -> &'static str {
        match self {
            Self::Timeout => "Timed out",
            Self::Voice => "Voice muted",
        }
    }

    /// What the role denies in every channel. Discord adds up the allows of all roles, so another
    /// role that allows one of these still wins; the sync counts those channels.
    fn denied(self) -> Permissions {
        match self {
            Self::Timeout => {
                Permissions::SEND_MESSAGES
                    | Permissions::SEND_MESSAGES_IN_THREADS
                    | Permissions::CREATE_PUBLIC_THREADS
                    | Permissions::CREATE_PRIVATE_THREADS
                    | Permissions::ADD_REACTIONS
                    | Permissions::SPEAK
                    | Permissions::STREAM
                    | Permissions::REQUEST_TO_SPEAK
            }
            Self::Voice => Permissions::SPEAK | Permissions::STREAM | Permissions::USE_VAD,
        }
    }

    /// The permission that the restriction takes away, which another role may still allow.
    fn speaking(self) -> Permissions {
        match self {
            Self::Timeout => Permissions::SEND_MESSAGES,
            Self::Voice => Permissions::SPEAK,
        }
    }
}

/// Why a dedicated role could not be had.
#[derive(Debug, thiserror::Error)]
pub enum RoleError {
    /// Discord refused, for instance because the bot lacks Manage Roles.
    #[error("discord refused the role: {0}")]
    Discord(#[from] DiscordError),
    /// The remembered id could not be read or written.
    #[error("the role could not be remembered: {0}")]
    Store(#[from] StorageError),
    /// The sync could not be scheduled, so the new role would stay without its channel settings.
    #[error("the role sync could not be scheduled: {0}")]
    Schedule(#[from] ScheduleError),
}

impl From<RoleError> for UserError {
    fn from(error: RoleError) -> Self {
        match error {
            RoleError::Discord(DiscordError::Forbidden(_)) => {
                UserError::sdk("punish-role-forbidden")
            }
            _ => UserError::sdk("punish-role-unavailable"),
        }
    }
}

/// The role a restriction gives. A configured role that exists is used as it is. Otherwise the
/// role Closure remembered, or one named like it, or else a new role with no permissions at all,
/// which is then set up in every channel by a sync job.
pub async fn ensure_role(
    ctx: &EventContext,
    which: RestrictionRole,
    configured: Option<RoleId>,
) -> Result<RoleId, RoleError> {
    let roles = roles(ctx).await?;
    let exists = |id: RoleId| roles.iter().any(|role| role.id == id);
    if let Some(role) = configured.filter(|role| exists(*role)) {
        return Ok(role);
    }
    let role = if let Some(role) = remembered(ctx, which).await?.filter(|role| exists(*role)) {
        role
    } else if let Some(found) = roles
        .iter()
        .find(|role| role.name.eq_ignore_ascii_case(which.role_name()))
    {
        remember(ctx, which, found.id).await?;
        found.id
    } else {
        let created = create(ctx, which).await?;
        remember(ctx, which, created).await?;
        created
    };
    ensure_synced(ctx, which, role).await?;
    Ok(role)
}

/// Schedules the channel sync of an auto role, unless a finished one is remembered for it. A sync
/// that could not be scheduled is tried again by the next punishment, which is refused until then.
async fn ensure_synced(
    ctx: &EventContext,
    which: RestrictionRole,
    role: RoleId,
) -> Result<(), RoleError> {
    let store = ctx.store();
    if store.get::<u64>(&synced_key(which)).await? == Some(role.get()) {
        return Ok(());
    }
    // A sync that is running (its lease is not out) is left to run: starting it again would set the
    // first channels again.
    if store.get::<u64>(&pending_key(which)).await? == Some(role.get()) {
        return Ok(());
    }
    let job = SyncJob {
        which,
        role: role.get(),
        after: 0,
        skipped: 0,
    };
    ctx.scheduler()
        .job(SYNC_JOB, &job)?
        .key(&sync_key(which))
        .after(Duration::ZERO)
        .await?;
    store
        .set_for(&pending_key(which), &role.get(), SYNC_LEASE)
        .await?;
    Ok(())
}

/// Sets the dedicated roles that are remembered on a channel that was just created, so that the
/// channel keeps the restrictions. A module calls this on `ChannelCreated`.
pub async fn channel_created(ctx: &EventContext, channel: ChannelId) -> Result<(), ModuleError> {
    for which in [RestrictionRole::Timeout, RestrictionRole::Voice] {
        let Some(role) = remembered(ctx, which)
            .await
            .map_err(|error| store_error(&error))?
        else {
            continue;
        };
        match set_overwrite(ctx, channel, which, role).await {
            Ok(()) | Err(DiscordError::NotFound | DiscordError::Forbidden(_)) => {}
            Err(error) => return Err(ModuleError::Discord(error)),
        }
    }
    Ok(())
}

/// The payload of the sync job: the restriction, its role, the last channel done and the channels
/// that could not be set up so far.
#[derive(Debug, Serialize, Deserialize)]
struct SyncJob {
    which: RestrictionRole,
    role: u64,
    after: u64,
    skipped: u32,
}

/// Sets the role's denies on the channels that have no overwrite for it, a batch per run. It runs
/// again, under the same key, until every channel is done, and then notes the outcome in the log.
pub(crate) async fn restriction_sync(cx: JobCx) -> Result<(), ModuleError> {
    let job: SyncJob = cx.payload()?;
    let Some(role) = RoleId::new_checked(job.role) else {
        return Err(ModuleError::other("a restriction sync names no role"));
    };
    let Outcome::Channels(mut channels) = cx
        .api()
        .execute(Action::ListChannels {
            guild_id: cx.guild_id(),
        })
        .await?
    else {
        return Err(ModuleError::other(
            "the channels of the guild were not listed",
        ));
    };
    channels.sort_unstable_by_key(|channel| channel.id.get());
    let unchecked = channels.len().saturating_sub(SYNC_CHANNELS);
    channels.truncate(SYNC_CHANNELS);

    let pending: Vec<&Channel> = channels
        .iter()
        .filter(|channel| channel.id.get() > job.after)
        .collect();
    let batch = &pending[..pending.len().min(SYNC_BATCH)];
    let mut skipped = job.skipped;
    for channel in batch {
        if has_overwrite(channel, role) {
            continue;
        }
        match set_overwrite(cx.ctx(), channel.id, job.which, role).await {
            Ok(()) => {}
            Err(DiscordError::Forbidden(_) | DiscordError::NotFound) => skipped += 1,
            Err(error) => return Err(ModuleError::Discord(error)),
        }
    }

    if let Some(last) = batch.last()
        && pending.len() > batch.len()
    {
        let next = SyncJob {
            which: job.which,
            role: job.role,
            after: last.id.get(),
            skipped,
        };
        cx.ctx()
            .scheduler()
            .job(SYNC_JOB, &next)
            .map_err(|error| ModuleError::other(error.to_string()))?
            .key(&sync_key(job.which))
            .after(Duration::ZERO)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
        return cx
            .ctx()
            .store()
            .set_for(&pending_key(job.which), &job.role, SYNC_LEASE)
            .await
            .map_err(|error| store_error(&error));
    }

    cx.ctx()
        .store()
        .set(&synced_key(job.which), &job.role)
        .await
        .map_err(|error| store_error(&error))?;
    cx.ctx()
        .store()
        .delete(&pending_key(job.which))
        .await
        .map_err(|error| store_error(&error))?;
    let allowed = channels
        .iter()
        .filter(|channel| allows_speaking(channel, role, job.which))
        .count();
    let args = Args::new()
        .with("skipped", count(skipped as usize))
        .with("allowed", count(allowed))
        .with("unchecked", count(unchecked));
    let text = cx
        .ctx()
        .t_core(cx.ctx().locale, "restriction-sync-done", &args);
    note(cx.ctx(), text).await
}

/// Removes the roles that `selection` names from `target`, except `dedicated`, and remembers them
/// for [`restore`]. Returns how many roles could not be removed.
pub(super) async fn strip(
    ctx: &EventContext,
    target: UserId,
    which: RestrictionRole,
    dedicated: RoleId,
    selection: &RoleSelection,
) -> Result<u32, ModuleError> {
    if matches!(selection, RoleSelection::None) {
        return Ok(0);
    }
    // Read before anything is taken off: a role that cannot be remembered must not be removed.
    let key = stripped_key(target, which);
    let mut kept: Vec<u64> = ctx
        .store()
        .get(&key)
        .await
        .map_err(|error| store_error(&error))?
        .unwrap_or_default();
    let member = match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: target,
        })
        .await
    {
        Ok(Outcome::Member(member)) => member,
        Err(DiscordError::NotFound) => return Ok(0),
        Err(error) => return Err(ModuleError::Discord(error)),
        Ok(_) => return Err(ModuleError::other("the member was not read")),
    };
    let guild_roles = roles(ctx).await.map_err(role_error)?;
    let bot_top = bot_top_position(ctx, &guild_roles).await?;
    let targets: Vec<RoleId> = member
        .roles
        .iter()
        .copied()
        .filter(|role| *role != dedicated)
        .filter(|role| match selection {
            RoleSelection::None => false,
            RoleSelection::Some(list) => list.contains(role),
            RoleSelection::All => guild_roles.iter().any(|known| {
                known.id == *role
                    && !known.managed
                    && known.id.get() != ctx.guild_id.get()
                    && known.position < bot_top
            }),
        })
        .take(STRIP_LIMIT)
        .collect();

    let mut removed = Vec::new();
    let mut skipped = 0;
    for role in targets {
        match ctx
            .api
            .remove_role(ctx.guild_id, target, role, audit(ctx, which, "stripped"))
            .await
        {
            Ok(()) => removed.push(role.get()),
            Err(
                DiscordError::Forbidden(_)
                | DiscordError::HierarchyViolation(_)
                | DiscordError::NotFound,
            ) => skipped += 1,
            Err(error) => return Err(ModuleError::Discord(error)),
        }
    }
    if !removed.is_empty() {
        for role in removed {
            if kept.len() < STRIP_LIMIT && !kept.contains(&role) {
                kept.push(role);
            }
        }
        ctx.store()
            .set(&key, &kept)
            .await
            .map_err(|error| store_error(&error))?;
    }
    Ok(skipped)
}

/// Gives back the roles [`strip`] removed from `target`, the ones `selection` names, and forgets
/// them. Returns how many could not be given back.
pub(super) async fn restore(
    ctx: &EventContext,
    target: UserId,
    which: RestrictionRole,
    selection: &RoleSelection,
) -> Result<u32, ModuleError> {
    let key = stripped_key(target, which);
    let stored: Vec<u64> = match ctx.store().get(&key).await {
        Ok(stored) => stored.unwrap_or_default(),
        // Without the storage grant nothing could have been stripped, so nothing is given back.
        Err(StorageError::NotGranted) => return Ok(0),
        Err(error) => return Err(store_error(&error)),
    };
    let wanted: Vec<RoleId> = match selection {
        RoleSelection::None => Vec::new(),
        RoleSelection::All => stored
            .iter()
            .filter_map(|id| RoleId::new_checked(*id))
            .collect(),
        RoleSelection::Some(list) => list
            .iter()
            .copied()
            .filter(|role| stored.contains(&role.get()))
            .collect(),
    };
    let mut skipped = 0;
    for role in wanted {
        match ctx
            .api
            .add_role(ctx.guild_id, target, role, audit(ctx, which, "restored"))
            .await
        {
            Ok(()) => {}
            Err(
                DiscordError::Forbidden(_)
                | DiscordError::HierarchyViolation(_)
                | DiscordError::NotFound,
            ) => skipped += 1,
            Err(error) => return Err(ModuleError::Discord(error)),
        }
    }
    if !stored.is_empty() {
        ctx.store()
            .delete(&key)
            .await
            .map_err(|error| store_error(&error))?;
    }
    Ok(skipped)
}

/// The key under which a member's stripped roles are remembered for a restriction.
#[must_use]
pub(super) fn stripped_key(user: UserId, which: RestrictionRole) -> String {
    format!("stripped:{}:{}", user.get(), which.key())
}

fn remembered_key(which: RestrictionRole) -> String {
    format!("auto-role:{}", which.key())
}

/// How long a running sync counts as running without a new run. A job that is lost ends its lease,
/// and the next punishment starts the sync again.
const SYNC_LEASE: Duration = Duration::from_secs(600);

/// The role whose sync is running, while its lease lasts.
fn pending_key(which: RestrictionRole) -> String {
    format!("pending:{}", which.key())
}

/// The role whose channels were all set up by the last finished sync of a restriction.
fn synced_key(which: RestrictionRole) -> String {
    format!("synced:{}", which.key())
}

fn sync_key(which: RestrictionRole) -> String {
    format!("{SYNC_JOB}:{}", which.key())
}

async fn roles(ctx: &EventContext) -> Result<Vec<Role>, DiscordError> {
    match ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await?
    {
        Outcome::Roles(roles) => Ok(roles),
        _ => Err(DiscordError::Fatal(
            "unexpected outcome for `list_roles`".into(),
        )),
    }
}

/// The highest position of the bot's own roles. Without the bot's member, nothing is held back
/// here and Discord refuses what is above the bot.
async fn bot_top_position(ctx: &EventContext, roles: &[Role]) -> Result<i64, ModuleError> {
    let Some(bot) = ctx.bot() else {
        return Ok(i64::MAX);
    };
    match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: bot,
        })
        .await
    {
        Ok(Outcome::Member(member)) => Ok(roles
            .iter()
            .filter(|role| member.roles.contains(&role.id))
            .map(|role| role.position)
            .max()
            .unwrap_or(-1)),
        Ok(_) | Err(DiscordError::NotFound) => Ok(i64::MAX),
        Err(error) => Err(ModuleError::Discord(error)),
    }
}

async fn remembered(
    ctx: &EventContext,
    which: RestrictionRole,
) -> Result<Option<RoleId>, StorageError> {
    Ok(ctx
        .store()
        .get::<u64>(&remembered_key(which))
        .await?
        .and_then(RoleId::new_checked))
}

async fn remember(
    ctx: &EventContext,
    which: RestrictionRole,
    role: RoleId,
) -> Result<(), StorageError> {
    ctx.store().set(&remembered_key(which), &role.get()).await
}

async fn create(ctx: &EventContext, which: RestrictionRole) -> Result<RoleId, DiscordError> {
    let reason = Reason::truncated(&ctx.t_core(
        ctx.locale,
        "role-created-reason",
        &Args::new().with("role", which.role_name()),
    ));
    match ctx
        .api
        .execute(Action::CreateRole {
            guild_id: ctx.guild_id,
            spec: NewRole {
                name: which.role_name().to_owned(),
                color: 0,
                permissions: Permissions::empty(),
                hoist: false,
                mentionable: false,
            },
            reason,
        })
        .await?
    {
        Outcome::Role(role) => Ok(role.id),
        _ => Err(DiscordError::Fatal(
            "unexpected outcome for `create_role`".into(),
        )),
    }
}

async fn set_overwrite(
    ctx: &EventContext,
    channel: ChannelId,
    which: RestrictionRole,
    role: RoleId,
) -> Result<(), DiscordError> {
    let reason = Reason::truncated(&ctx.t_core(
        ctx.locale,
        "restriction-sync-reason",
        &Args::new().with("role", which.role_name()),
    ));
    ctx.api
        .execute(Action::SetPermissionOverwrite {
            channel_id: channel,
            overwrite: OverwriteSpec {
                target: OverwriteTarget::Role(role),
                allow: Permissions::empty(),
                deny: which.denied(),
            },
            reason,
        })
        .await
        .map(drop)
}

fn has_overwrite(channel: &Channel, role: RoleId) -> bool {
    channel
        .permission_overwrites
        .iter()
        .flatten()
        .any(|overwrite| {
            overwrite.kind == PermissionOverwriteType::Role && overwrite.id.get() == role.get()
        })
}

/// Whether another role, @everyone included, allows what the restriction takes away here.
fn allows_speaking(channel: &Channel, role: RoleId, which: RestrictionRole) -> bool {
    channel
        .permission_overwrites
        .iter()
        .flatten()
        .any(|overwrite| {
            overwrite.kind == PermissionOverwriteType::Role
                && overwrite.id.get() != role.get()
                && overwrite.allow.contains(which.speaking())
        })
}

fn audit(ctx: &EventContext, which: RestrictionRole, what: &str) -> Reason {
    Reason::truncated(
        &ctx.t_core(
            ctx.locale,
            "restriction-audit",
            &Args::new()
                .with("role", which.role_name())
                .with("what", what),
        ),
    )
}

async fn note(ctx: &EventContext, text: String) -> Result<(), ModuleError> {
    let Some(channel) = ctx.policy().log_channel else {
        return Ok(());
    };
    ctx.api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: Some(text),
            embeds: Vec::new(),
            reply_to: None,
            mentions: MentionPolicy::None,
        })
        .await
        .map(drop)
        .map_err(ModuleError::Discord)
}

fn count(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn store_error(error: &StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn role_error(error: DiscordError) -> ModuleError {
    ModuleError::Discord(error)
}
