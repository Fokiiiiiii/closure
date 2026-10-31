//! The dedicated roles of the role method. A timeout or a voice mute that gives a role uses one
//! that Closure creates on first use, or finds by name, and sets up in every channel of the guild
//! so that the role takes the speech away there. An explicitly configured role is never touched.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex, OnceLock, PoisonError};
use std::time::Duration;

use closure_core::action::{
    Action, NewRole, Outcome, OverwriteSpec, OverwriteTarget, Reason, RolePatch,
};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, RoleId, UserId};
use closure_core::locale::Args;
use closure_core::module::EventContext;
use closure_core::schedule::ScheduleError;
use closure_core::storage::StorageError;
use futures_util::lock::Mutex;
use serde::{Deserialize, Serialize};
use twilight_model::channel::Channel;
use twilight_model::channel::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType};
use twilight_model::guild::{Permissions, Role};

use super::{RoleSelection, STRIP_LIMIT, active_cases};
use crate::embed::{Color, DESCRIPTION_LIMIT, EmbedBuilder, truncate};
use crate::{JobCx, UserError};

/// The job that sets a dedicated role up in the channels, a batch of them per run.
const SYNC_JOB: &str = "restriction_sync";
/// The channels one run of the sync sets up.
const SYNC_BATCH: usize = 10;
/// The channels the sync looks at, lowest id first. shortcut: a guild with more channels than this
/// leaves the rest unchecked, which the log note says; upgrade by paging the channel list.
const SYNC_CHANNELS: usize = 500;
/// How many times a sync that could not set every channel up is run again.
const SYNC_RETRIES: u32 = 3;
/// How long after a sync that left channels undone the next run starts.
const SYNC_RETRY_DELAY: Duration = Duration::from_secs(3_600);
/// How long roles that could not be given back stay remembered for a later lift.
const RESTORE_KEEP: Duration = Duration::from_secs(30 * 24 * 3_600);
/// Every kind of restriction role, for the lookups that cover all of them.
const RESTRICTIONS: [RestrictionRole; 3] = [
    RestrictionRole::Timeout,
    RestrictionRole::Voice,
    RestrictionRole::Quarantine,
];

/// Which restriction a dedicated role stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestrictionRole {
    /// A timeout.
    Timeout,
    /// A voice mute.
    Voice,
    /// A member held back by an antiraid level: no speaking, and no reactions or threads.
    Quarantine,
}

impl RestrictionRole {
    /// The part of store keys and job keys that tells the two apart.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Voice => "vcmute",
            Self::Quarantine => "quarantine",
        }
    }

    /// The catalog key of the name a created role has.
    const fn name_key(self) -> &'static str {
        match self {
            Self::Timeout => "role-name-timeout",
            Self::Voice => "role-name-voice",
            Self::Quarantine => "role-name-quarantine",
        }
    }

    /// The name a created role has, in the guild's language. Found by name without regard to case, so
    /// that two modules in one guild share one role.
    #[must_use]
    pub fn role_name(self, ctx: &EventContext) -> String {
        ctx.t_core(ctx.locale, self.name_key(), &Args::new())
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
            Self::Quarantine => {
                Permissions::SEND_MESSAGES
                    | Permissions::SEND_MESSAGES_IN_THREADS
                    | Permissions::CREATE_PUBLIC_THREADS
                    | Permissions::CREATE_PRIVATE_THREADS
                    | Permissions::ADD_REACTIONS
                    | Permissions::SPEAK
                    | Permissions::STREAM
            }
        }
    }

    /// The permission that the restriction takes away, which another role may still allow.
    fn speaking(self) -> Permissions {
        match self {
            Self::Timeout | Self::Quarantine => Permissions::SEND_MESSAGES,
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

/// The lock of one guild and restriction kind. Two callers in one process take turns on it, so
/// that the second finds the role the first made. Several bot processes are not covered.
type Gate = Arc<Mutex<()>>;

fn gates() -> &'static StdMutex<HashMap<(u64, &'static str), Gate>> {
    static GATES: OnceLock<StdMutex<HashMap<(u64, &'static str), Gate>>> = OnceLock::new();
    GATES.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// The turns of the role changes of each member, by guild and user. A gate is kept only while a
/// turn is taken or waited for.
fn member_gates() -> &'static StdMutex<HashMap<(u64, u64), Gate>> {
    static MEMBER_GATES: OnceLock<StdMutex<HashMap<(u64, u64), Gate>>> = OnceLock::new();
    MEMBER_GATES.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// Runs `work` while no other strip or restore of `target` runs. Each of them reads the plan of the
/// member, makes Discord calls and writes the plan back, so they must not interleave.
async fn one_at_a_time<T>(ctx: &EventContext, target: UserId, work: impl Future<Output = T>) -> T {
    let key = (ctx.guild_id.get(), target.get());
    let gate = {
        let mut map = member_gates()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::clone(map.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
    };
    let result = {
        let _turn = gate.lock().await;
        work.await
    };
    // Only the map and this call hold the gate when nobody waits for it: it is dropped then.
    let mut map = member_gates()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if Arc::strong_count(&gate) <= 2 {
        map.remove(&key);
    }
    result
}

/// The role a restriction gives. A configured role that exists is used as it is. Otherwise the
/// role Closure remembered, or one named like it, or else a new role with no permissions at all,
/// which is then set up in every channel by a sync job. A role is only adopted when it is safe to
/// hand to a punished member: see [`is_safe`].
pub async fn ensure_role(
    ctx: &EventContext,
    which: RestrictionRole,
    configured: Option<RoleId>,
) -> Result<RoleId, RoleError> {
    let key = (ctx.guild_id.get(), which.key());
    let gate = {
        let mut map = gates().lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(map.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
    };
    let result = {
        let _turn = gate.lock().await;
        ensure_role_now(ctx, which, configured).await
    };
    // Only the map and this call hold the gate when nobody waits for it: it is dropped then.
    let mut map = gates().lock().unwrap_or_else(PoisonError::into_inner);
    if Arc::strong_count(&gate) <= 2 {
        map.remove(&key);
    }
    result
}

/// Gives a remembered role the name and the colour of now, when they differ: a role made before the names
/// were in the guild's language, or before the roles were red. A failure is logged, and the role is used as
/// it is.
async fn refresh(ctx: &EventContext, roles: &[Role], role: RoleId, name: &str) {
    let Some(current) = roles.iter().find(|current| current.id == role) else {
        return;
    };
    let colour = Color::Error.value();
    if current.name == name && current.colors.primary_color == colour {
        return;
    }
    let patch = RolePatch {
        name: Some(name.to_owned()),
        color: Some(colour),
        ..RolePatch::default()
    };
    let reason = Reason::truncated(&ctx.t_core(ctx.locale, "role-updated-reason", &Args::new()));
    if let Err(error) = ctx
        .api
        .execute(Action::EditRole {
            guild_id: ctx.guild_id,
            role_id: role,
            patch,
            reason,
        })
        .await
    {
        tracing::warn!(role = role.get(), %error, "could not update the name and colour of a restriction role");
    }
}

async fn ensure_role_now(
    ctx: &EventContext,
    which: RestrictionRole,
    configured: Option<RoleId>,
) -> Result<RoleId, RoleError> {
    let roles = roles(ctx).await?;
    let exists = |id: RoleId| roles.iter().any(|role| role.id == id);
    if let Some(role) = configured.filter(|role| exists(*role)) {
        // The explicit role is the one in use now, so the auto role is no longer channel-managed.
        match ctx.store().delete(&remembered_key(which)).await {
            Ok(_) | Err(StorageError::NotGranted) => {}
            Err(error) => return Err(RoleError::Store(error)),
        }
        return Ok(role);
    }
    let bot_top = bot_top_position(ctx, &roles).await?;
    let safe = |role: &Role| is_safe(ctx, role, bot_top);
    let kept = remembered(ctx, which)
        .await?
        .filter(|id| roles.iter().any(|role| role.id == *id && safe(role)));
    let name = which.role_name(ctx);
    let role = if let Some(role) = kept {
        refresh(ctx, &roles, role, &name).await;
        role
    } else if let Some(found) = roles
        .iter()
        .find(|role| role.name.eq_ignore_ascii_case(&name) && safe(role))
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

/// Whether a role may be adopted for a punishment: no permissions at all, not managed by an
/// integration, not @everyone, and below the bot's top role so that the bot can give it.
fn is_safe(ctx: &EventContext, role: &Role, bot_top: i64) -> bool {
    role.permissions == Permissions::empty()
        && !role.managed
        && role.id.get() != ctx.guild_id.get()
        && role.position < bot_top
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
    let mut dedicated = Vec::new();
    for which in RESTRICTIONS {
        match remembered(ctx, which).await {
            Ok(Some(role)) => dedicated.push((which, role)),
            Ok(None) => {}
            Err(error) => return Err(store_error(&error)),
        }
    }
    if dedicated.is_empty() {
        return Ok(());
    }
    let Some(existing) = channel_overwrites(ctx, channel).await? else {
        return Ok(());
    };
    for (which, role) in dedicated {
        let overwrite = overwrite_of(existing.iter(), role);
        if is_set(overwrite, which) {
            continue;
        }
        match set_overwrite(ctx, channel, which, role, overwrite).await {
            Ok(()) | Err(DiscordError::NotFound | DiscordError::Forbidden(_)) => {}
            Err(error) => return Err(ModuleError::Discord(error)),
        }
    }
    Ok(())
}

/// The overwrites a channel has now. `None` when the channel cannot be read: then nothing is set,
/// since the overwrites that are there would be lost.
async fn channel_overwrites(
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<Option<Vec<PermissionOverwrite>>, ModuleError> {
    match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await
    {
        Ok(Outcome::Channel(channel)) => {
            Ok(Some(channel.permission_overwrites.unwrap_or_default()))
        }
        Ok(_) => Ok(Some(Vec::new())),
        Err(DiscordError::NotFound | DiscordError::Forbidden(_)) => Ok(None),
        Err(error) => Err(ModuleError::Discord(error)),
    }
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

/// Sets the role's denies on the channels that do not deny them yet, a batch per run. It runs
/// again, under the same key, until every channel is done. A run that left channels undone is
/// tried again later, a bounded number of times, before the outcome is noted in the log.
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
    let skipped = set_batch(&cx, &job, role, batch).await?;

    if let Some(last) = batch.last()
        && pending.len() > batch.len()
    {
        return schedule_batch(&cx, &job, last.id.get(), skipped).await;
    }
    if skipped > 0 && schedule_retry(&cx, &job).await? {
        return Ok(());
    }
    finish(&cx, &job, role, &channels, skipped, unchecked).await
}

/// Sets the role's denies on the channels of one batch that do not deny them yet. Returns how many
/// channels Discord refused, the ones of earlier runs included.
async fn set_batch(
    cx: &JobCx,
    job: &SyncJob,
    role: RoleId,
    batch: &[&Channel],
) -> Result<u32, ModuleError> {
    let mut skipped = job.skipped;
    for channel in batch {
        let existing = overwrite_of(channel.permission_overwrites.iter().flatten(), role);
        if is_set(existing, job.which) {
            continue;
        }
        match set_overwrite(cx.ctx(), channel.id, job.which, role, existing).await {
            Ok(()) => {}
            Err(DiscordError::Forbidden(_) | DiscordError::NotFound) => skipped += 1,
            Err(error) => return Err(ModuleError::Discord(error)),
        }
    }
    Ok(skipped)
}

/// Schedules the next batch of the sync under the same key, and holds the lease while it waits.
async fn schedule_batch(
    cx: &JobCx,
    job: &SyncJob,
    after: u64,
    skipped: u32,
) -> Result<(), ModuleError> {
    let next = SyncJob {
        which: job.which,
        role: job.role,
        after,
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
    cx.ctx()
        .store()
        .set_for(&pending_key(job.which), &job.role, SYNC_LEASE)
        .await
        .map_err(|error| store_error(&error))
}

/// Schedules a run of the sync from its start, an hour later, if a retry is left. Returns whether
/// one was scheduled.
async fn schedule_retry(cx: &JobCx, job: &SyncJob) -> Result<bool, ModuleError> {
    let store = cx.ctx().store();
    let tried: u32 = store
        .get(&retries_key(job.which))
        .await
        .map_err(|error| store_error(&error))?
        .unwrap_or(0);
    if tried >= SYNC_RETRIES {
        return Ok(false);
    }
    store
        .set(&retries_key(job.which), &(tried + 1))
        .await
        .map_err(|error| store_error(&error))?;
    store
        .delete(&pending_key(job.which))
        .await
        .map_err(|error| store_error(&error))?;
    let again = SyncJob {
        which: job.which,
        role: job.role,
        after: 0,
        skipped: 0,
    };
    cx.ctx()
        .scheduler()
        .job(SYNC_JOB, &again)
        .map_err(|error| ModuleError::other(error.to_string()))?
        .key(&sync_key(job.which))
        .after(SYNC_RETRY_DELAY)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(true)
}

/// Marks the sync of a restriction done and notes its outcome in the log.
async fn finish(
    cx: &JobCx,
    job: &SyncJob,
    role: RoleId,
    channels: &[Channel],
    skipped: u32,
    unchecked: usize,
) -> Result<(), ModuleError> {
    let store = cx.ctx().store();
    store
        .set(&synced_key(job.which), &job.role)
        .await
        .map_err(|error| store_error(&error))?;
    store
        .delete(&retries_key(job.which))
        .await
        .map_err(|error| store_error(&error))?;
    store
        .delete(&pending_key(job.which))
        .await
        .map_err(|error| store_error(&error))?;
    let guild = cx.guild_id().get();
    let allowed = channels
        .iter()
        .filter(|channel| allows_speaking(channel, role, job.which, guild))
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

/// The roles that a restriction takes off a member: every role of `selection` except the
/// restriction roles, which are never taken off, and only up to [`STRIP_LIMIT`] remembered ones.
/// The roles taken off are saved before the first removal, so a removal that fails midway keeps
/// what was taken off. Returns how many roles could not be taken off.
pub(super) async fn strip(
    ctx: &EventContext,
    target: UserId,
    which: RestrictionRole,
    dedicated: RoleId,
    configured: &[RoleId],
    selection: &RoleSelection,
) -> Result<u32, ModuleError> {
    one_at_a_time(
        ctx,
        target,
        strip_now(ctx, target, which, dedicated, configured, selection),
    )
    .await
}

async fn strip_now(
    ctx: &EventContext,
    target: UserId,
    which: RestrictionRole,
    dedicated: RoleId,
    configured: &[RoleId],
    selection: &RoleSelection,
) -> Result<u32, ModuleError> {
    if matches!(selection, RoleSelection::None) {
        return Ok(0);
    }
    // Read before anything is taken off: a role that cannot be remembered must not be removed.
    let mut plan = stripped(ctx, target)
        .await
        .map_err(|error| store_error(&error))?;
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
    let bot_top = bot_top_position(ctx, &guild_roles)
        .await
        .map_err(ModuleError::Discord)?;
    let mut restricted = restriction_ids(ctx, configured)
        .await
        .map_err(|error| store_error(&error))?;
    restricted.push(dedicated);
    let targets: Vec<RoleId> = member
        .roles
        .iter()
        .copied()
        .filter(|role| !restricted.contains(role))
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
        .collect();

    // Roles beyond the limit stay on the member, and are counted as not taken off.
    let mut skipped = 0;
    let mut chosen = Vec::new();
    for role in targets {
        if plan.contains(&role.get()) {
            chosen.push(role);
        } else if plan.len() < STRIP_LIMIT {
            plan.push(role.get());
            chosen.push(role);
        } else {
            skipped += 1;
        }
    }
    if chosen.is_empty() {
        return Ok(skipped);
    }
    // Saved before the first removal: a failure midway then keeps the ones already taken off.
    ctx.restrictions()
        .set(&stripped_key(target), &plan)
        .await
        .map_err(|error| store_error(&error))?;
    forget_older(ctx, target)
        .await
        .map_err(|error| store_error(&error))?;

    let mut failed = Vec::new();
    for role in chosen {
        // A role that cannot be taken off, for any reason, is counted and noted, and the others
        // still go. Whatever is saved and not removed is harmless: giving it back is idempotent.
        if ctx
            .api
            .remove_role(ctx.guild_id, target, role, audit(ctx, which, "stripped"))
            .await
            .is_err()
        {
            skipped += 1;
            failed.push(role.get());
        }
    }
    if !failed.is_empty() {
        plan.retain(|id| !failed.contains(id));
        ctx.restrictions()
            .set(&stripped_key(target), &plan)
            .await
            .map_err(|error| store_error(&error))?;
    }
    Ok(skipped)
}

/// Gives back the roles that [`strip`] took off `target` and that `selection` names. Those that
/// could not be given back stay remembered for [`RESTORE_KEEP`], so that a later lift can finish;
/// a role that no longer exists is dropped. Returns how many could not be given back.
pub(super) async fn restore(
    ctx: &EventContext,
    target: UserId,
    which: RestrictionRole,
    selection: &RoleSelection,
) -> Result<u32, ModuleError> {
    one_at_a_time(ctx, target, restore_now(ctx, target, which, selection)).await
}

async fn restore_now(
    ctx: &EventContext,
    target: UserId,
    which: RestrictionRole,
    selection: &RoleSelection,
) -> Result<u32, ModuleError> {
    let stored = match stripped(ctx, target).await {
        Ok(stored) => stored,
        // Without the storage grant nothing could have been stripped, so nothing is given back.
        Err(StorageError::NotGranted) => return Ok(0),
        Err(error) => return Err(store_error(&error)),
    };
    // A restriction role is never given back, whatever the lists say.
    let restricted: Vec<u64> = restriction_ids(ctx, &[])
        .await
        .map_err(|error| store_error(&error))?
        .into_iter()
        .map(RoleId::get)
        .collect();
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
    let pending: Vec<RoleId> = wanted
        .into_iter()
        .filter(|role| !restricted.contains(&role.get()))
        .collect();
    let mut given = Vec::new();
    let mut gone = Vec::new();
    let mut kept = 0;
    let mut fatal = None;
    for role in pending.iter().copied() {
        match ctx
            .api
            .add_role(ctx.guild_id, target, role, audit(ctx, which, "restored"))
            .await
        {
            Ok(()) => given.push(role.get()),
            // NotFound may mean the member is gone, which is kept for a later lift: only a role
            // that no longer exists is dropped.
            Err(DiscordError::NotFound) => {
                if roles(ctx)
                    .await
                    .map_err(role_error)?
                    .iter()
                    .any(|known| known.id == role)
                {
                    kept += 1;
                } else {
                    gone.push(role.get());
                }
            }
            Err(DiscordError::Forbidden(_) | DiscordError::HierarchyViolation(_)) => kept += 1,
            Err(error) => {
                fatal = Some(error);
                break;
            }
        }
    }
    // Only the roles this restore chose are remembered, and only those it did not give back yet.
    let left: Vec<u64> = pending
        .iter()
        .map(|role| role.get())
        .filter(|id| !given.contains(id) && !gone.contains(id))
        .collect();
    if left.is_empty() {
        ctx.restrictions()
            .delete(&stripped_key(target))
            .await
            .map_err(|error| store_error(&error))?;
    } else {
        ctx.restrictions()
            .set_for(&stripped_key(target), &left, RESTORE_KEEP)
            .await
            .map_err(|error| store_error(&error))?;
    }
    forget_older(ctx, target)
        .await
        .map_err(|error| store_error(&error))?;
    if let Some(error) = fatal {
        return Err(ModuleError::Discord(error));
    }
    Ok(kept)
}

/// Gives back the roles of `target` once no restriction of any kind still stands for it, in any
/// module. `lifting` is how many standing cases this lift takes away: one for a manual lift of a
/// standing case, none for a timer whose case has run out.
pub(super) async fn restore_when_free(
    ctx: &EventContext,
    target: UserId,
    which: RestrictionRole,
    lifting: usize,
    selection: &RoleSelection,
) -> Result<(), ModuleError> {
    let mut standing = 0;
    for kind in ["timeout", "vcmute"] {
        standing += active_cases(ctx.cases(), target, kind, ctx.now(), lifting + 1)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?
            .len();
    }
    if standing > lifting {
        return Ok(());
    }
    restore(ctx, target, which, selection).await.map(drop)
}

/// The key under which the roles a member lost to restrictions are remembered, shared by all of
/// them: a timeout and a voice mute of one member take and give roles together.
#[must_use]
fn stripped_key(user: UserId) -> String {
    format!("stripped:{}", user.get())
}

/// The keys the roles were remembered under before the list was shared. They are read, and
/// removed once their roles are in [`stripped_key`].
fn older_stripped_keys(user: UserId) -> [String; 2] {
    [
        format!("stripped:{}:timeout", user.get()),
        format!("stripped:{}:vcmute", user.get()),
    ]
}

/// The roles remembered for a member: the list every module shares, and the ones a module kept in
/// its own storage before the list was shared (see [`forget_older`]).
async fn stripped(ctx: &EventContext, user: UserId) -> Result<Vec<u64>, StorageError> {
    let mut all: Vec<u64> = ctx
        .restrictions()
        .get(&stripped_key(user))
        .await?
        .unwrap_or_default();
    let mut own = vec![stripped_key(user)];
    own.extend(older_stripped_keys(user));
    for key in own {
        for id in ctx.store().get::<Vec<u64>>(&key).await?.unwrap_or_default() {
            if !all.contains(&id) {
                all.push(id);
            }
        }
    }
    Ok(all)
}

/// Forgets the roles a module kept in its own storage under the keys before the list was shared:
/// they are in the shared list now, which every module reads.
async fn forget_older(ctx: &EventContext, user: UserId) -> Result<(), StorageError> {
    ctx.store().delete(&stripped_key(user)).await?;
    for key in older_stripped_keys(user) {
        ctx.store().delete(&key).await?;
    }
    Ok(())
}

/// How long a running sync counts as running without a new run. A job that is lost ends its lease,
/// and the next punishment starts the sync again.
const SYNC_LEASE: Duration = Duration::from_secs(600);

fn remembered_key(which: RestrictionRole) -> String {
    format!("auto-role:{}", which.key())
}

/// The role whose sync is running, while its lease lasts.
fn pending_key(which: RestrictionRole) -> String {
    format!("pending:{}", which.key())
}

/// The role whose channels were all set up by the last finished sync of a restriction.
fn synced_key(which: RestrictionRole) -> String {
    format!("synced:{}", which.key())
}

/// How many runs of the sync of a restriction have been retried since it last finished.
fn retries_key(which: RestrictionRole) -> String {
    format!("sync-retries:{}", which.key())
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
async fn bot_top_position(ctx: &EventContext, roles: &[Role]) -> Result<i64, DiscordError> {
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
        Err(error) => Err(error),
    }
}

/// Every restriction role the module knows of: the configured ones and the remembered ones of all
/// kinds. These are never taken off a member and never given back.
async fn restriction_ids(
    ctx: &EventContext,
    configured: &[RoleId],
) -> Result<Vec<RoleId>, StorageError> {
    let mut ids = configured.to_vec();
    for which in RESTRICTIONS {
        if let Some(role) = remembered(ctx, which).await? {
            ids.push(role);
        }
    }
    Ok(ids)
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
    let name = which.role_name(ctx);
    let reason = Reason::truncated(&ctx.t_core(
        ctx.locale,
        "role-created-reason",
        &Args::new().with("role", name.as_str()),
    ));
    match ctx
        .api
        .execute(Action::CreateRole {
            guild_id: ctx.guild_id,
            spec: NewRole {
                name,
                // Red, as the errors are: a restriction shows at a glance.
                color: Color::Error.value(),
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

/// Sets the restriction's denies on the role's overwrite in a channel. The bits the channel
/// already has stay, except that our denies take over any allow of the same bits.
async fn set_overwrite(
    ctx: &EventContext,
    channel: ChannelId,
    which: RestrictionRole,
    role: RoleId,
    existing: Option<&PermissionOverwrite>,
) -> Result<(), DiscordError> {
    let ours = which.denied();
    let (allow, deny) = existing
        .map_or((Permissions::empty(), Permissions::empty()), |overwrite| {
            (overwrite.allow, overwrite.deny)
        });
    let reason = Reason::truncated(&ctx.t_core(
        ctx.locale,
        "restriction-sync-reason",
        &Args::new().with("role", which.role_name(ctx)),
    ));
    ctx.api
        .execute(Action::SetPermissionOverwrite {
            channel_id: channel,
            overwrite: OverwriteSpec {
                target: OverwriteTarget::Role(role),
                allow: allow & !ours,
                deny: deny | ours,
            },
            reason,
        })
        .await
        .map(drop)
}

/// The overwrite of `role` among `overwrites`, if the channel has one.
fn overwrite_of<'a>(
    overwrites: impl Iterator<Item = &'a PermissionOverwrite>,
    role: RoleId,
) -> Option<&'a PermissionOverwrite> {
    overwrites.into_iter().find(|overwrite| {
        overwrite.kind == PermissionOverwriteType::Role && overwrite.id.get() == role.get()
    })
}

/// Whether the overwrite already denies everything the restriction denies, so that nothing needs
/// setting in its channel.
fn is_set(existing: Option<&PermissionOverwrite>, which: RestrictionRole) -> bool {
    existing.is_some_and(|overwrite| overwrite.deny.contains(which.denied()))
}

/// Whether another role or a member allows what the restriction takes away here. @everyone's
/// allow does not count: the restriction's own deny is applied after it.
fn allows_speaking(channel: &Channel, role: RoleId, which: RestrictionRole, guild: u64) -> bool {
    channel
        .permission_overwrites
        .iter()
        .flatten()
        .any(|overwrite| {
            let other = match overwrite.kind {
                PermissionOverwriteType::Role => {
                    overwrite.id.get() != role.get() && overwrite.id.get() != guild
                }
                PermissionOverwriteType::Member => true,
                _ => false,
            };
            other && overwrite.allow.contains(which.speaking())
        })
}

fn audit(ctx: &EventContext, which: RestrictionRole, what: &str) -> Reason {
    Reason::truncated(
        &ctx.t_core(
            ctx.locale,
            "restriction-audit",
            &Args::new()
                .with("role", which.role_name(ctx))
                .with("what", what),
        ),
    )
}

async fn note(ctx: &EventContext, text: String) -> Result<(), ModuleError> {
    let Some(channel) = ctx.policy().log_channel else {
        return Ok(());
    };
    let embed = EmbedBuilder::new()
        .description(truncate(&text, DESCRIPTION_LIMIT))
        .color(Color::Info)
        .build()
        .map_err(|error| ModuleError::other(error.to_string()))?;
    ctx.api
        .send_embeds(channel, vec![embed])
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
