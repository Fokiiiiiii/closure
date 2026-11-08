//! Antiraid: the levels a server raises while a raid runs, what each level changes, and the
//! quarantine of the members who join at a level that has one.
//!
//! The level is one record in the store, and [`set_level`] is the only place it changes: the
//! command, a rule's action and the job that ends a timed level all use it. Each change is applied
//! by comparing with what the module changed before, so that a later level restores what an earlier
//! one set (a slowmode) or paused (the invites).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use closure_core::action::{Action, ChannelPatch, GuildPatch, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_sdk::embed::{Color, DESCRIPTION_LIMIT, EmbedBuilder, truncate};
use closure_sdk::punish::{RestrictionRole, ensure_role, member_turn};
use closure_sdk::{
    Args, ChannelId, Event, EventContext, Expect, GuildId, JobCx, ModuleError, RoleId,
    StorageError, Store, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use twilight_model::channel::{Channel, ChannelType};
use twilight_model::guild::GuildFeature;

use crate::actions::text;
use crate::config::Problem;
use crate::context::Context;
use crate::engine::{self, State};
use crate::memory::Memory;

/// The levels a server has when it names none: the first is the calm one.
const DEFAULT_LEVELS: [&str; 5] = ["none", "low", "medium", "high", "lockdown"];
const MAX_LEVELS: usize = 10;
const MAX_LEVEL_CHARS: usize = 32;
/// Discord's longest slowmode.
const MAX_SLOWMODE_SECONDS: u16 = 21_600;
const MAX_EFFECT_CHANNELS: usize = 100;
/// The text channels a slowmode with no channel list reaches; the rest of the guild is left alone.
const MAX_CHANNELS: usize = 200;
/// The first format of the ledger kept the members in chunks of a store value; they are read once
/// to move them into sets.
const LEGACY_CHUNKS: usize = 17;
/// How often a write that lost a race to another write is tried again.
const CAS_TRIES: usize = 5;
pub const EXPIRE_JOB: &str = "antiraid_expire";
const EXPIRE_KEY: &str = "antiraid";
/// The job that applies again the effects of a change that were not all made. It has one key, so
/// that a later try replaces an earlier one.
pub const RESUME_JOB: &str = "antiraid_resume";
const RESUME_KEY: &str = "antiraid:resume";
const RESUME_DELAY: Duration = Duration::from_secs(60);
const RESUME_TRIES: u32 = 5;
const LEVEL_KEY: &str = "antiraid:level";
const INVITES_KEY: &str = "antiraid:invites";
const SLOWMODE_KEY: &str = "antiraid:slowmode:";
const QUARANTINE_KEY: &str = "antiraid:quarantined:";
/// The marker that the quarantine role sync of the SDK stores once the channels of the role are set.
const QUARANTINE_SYNCED_KEY: &str = "synced:quarantine";
const LIST_LIMIT: usize = 1_000;

/// The antiraid section as written. It is read on its own, so that a bad section disables antiraid
/// and nothing else.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AntiraidConfig {
    /// Off keeps the settings but runs nothing; left out, it is on.
    pub enabled: Option<bool>,
    pub levels: Option<Vec<String>>,
    pub effects: BTreeMap<String, EffectConfig>,
    pub quarantine_release: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EffectConfig {
    pub slowmode: Option<SlowmodeConfig>,
    pub pause_invites: bool,
    pub quarantine: Option<QuarantineConfig>,
    pub notify_channel: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowmodeConfig {
    #[serde(default)]
    pub channels: Vec<String>,
    pub seconds: u16,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QuarantineConfig {
    pub role: Option<String>,
    pub marker_role: Option<String>,
}

/// The antiraid section, checked. Its absence means antiraid is off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Antiraid {
    /// In order; the first is the calm level.
    pub levels: Vec<String>,
    pub effects: BTreeMap<String, Effect>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effect {
    pub slowmode: Option<Slowmode>,
    pub pause_invites: bool,
    pub quarantine: Option<Quarantine>,
    pub notify: Option<ChannelId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slowmode {
    /// Empty means every text channel of the guild, up to [`MAX_CHANNELS`].
    pub channels: Vec<ChannelId>,
    pub seconds: u16,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Quarantine {
    /// `None` means the quarantine role Closure creates.
    pub role: Option<RoleId>,
    pub marker: Option<RoleId>,
}

impl Antiraid {
    #[must_use]
    pub fn calm(&self) -> &str {
        // `check` keeps at least two levels, so the first always exists.
        self.levels.first().map_or("", String::as_str)
    }
}

/// Reads the antiraid section when there is one. A section that is not valid is a problem, and
/// antiraid is then off; the rules are not affected.
pub fn read(raw: Option<&Value>) -> Result<Option<Antiraid>, Problem> {
    let Some(raw) = raw.filter(|raw| !raw.is_null()) else {
        return Ok(None);
    };
    let config: AntiraidConfig =
        serde_json::from_value(raw.clone()).map_err(|error| Problem::Shape(error.to_string()))?;
    if config.enabled == Some(false) {
        return Ok(None);
    }
    check(config).map(Some)
}

fn check(config: AntiraidConfig) -> Result<Antiraid, Problem> {
    if config.quarantine_release {
        return Err(Problem::Key("problem-antiraid-release"));
    }
    let levels: Vec<String> = config.levels.unwrap_or_else(|| {
        DEFAULT_LEVELS
            .iter()
            .map(|level| (*level).to_owned())
            .collect()
    });
    let unique: BTreeSet<&String> = levels.iter().collect();
    if !(2..=MAX_LEVELS).contains(&levels.len())
        || unique.len() != levels.len()
        || !levels.iter().all(|level| valid_level(level))
    {
        return Err(Problem::Key("problem-antiraid-levels"));
    }
    let mut effects = BTreeMap::new();
    for (name, effect) in config.effects {
        if !levels.contains(&name) {
            return Err(Problem::Key("problem-antiraid-effect"));
        }
        effects.insert(name, check_effect(&effect)?);
    }
    Ok(Antiraid { levels, effects })
}

/// A level name is 1 to 32 characters of lowercase letters, digits, `_` and `-`.
#[must_use]
pub fn valid_level(name: &str) -> bool {
    (1..=MAX_LEVEL_CHARS).contains(&name.len())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

fn check_effect(effect: &EffectConfig) -> Result<Effect, Problem> {
    Ok(Effect {
        slowmode: effect.slowmode.as_ref().map(check_slowmode).transpose()?,
        pause_invites: effect.pause_invites,
        quarantine: effect
            .quarantine
            .as_ref()
            .map(check_quarantine)
            .transpose()?,
        notify: effect
            .notify_channel
            .as_deref()
            .map(channel_id)
            .transpose()?,
    })
}

fn check_slowmode(slowmode: &SlowmodeConfig) -> Result<Slowmode, Problem> {
    if slowmode.seconds > MAX_SLOWMODE_SECONDS || slowmode.channels.len() > MAX_EFFECT_CHANNELS {
        return Err(Problem::Key("problem-antiraid-slowmode"));
    }
    Ok(Slowmode {
        channels: channel_ids(&slowmode.channels)?,
        seconds: slowmode.seconds,
    })
}

fn check_quarantine(quarantine: &QuarantineConfig) -> Result<Quarantine, Problem> {
    Ok(Quarantine {
        role: optional_role(quarantine.role.as_deref())?,
        marker: optional_role(quarantine.marker_role.as_deref())?,
    })
}

fn snowflake(text: &str) -> Option<u64> {
    text.parse::<u64>().ok().filter(|id| *id > 0)
}

fn channel_id(text: &str) -> Result<ChannelId, Problem> {
    snowflake(text)
        .and_then(ChannelId::new_checked)
        .ok_or(Problem::Key("problem-antiraid-id"))
}

fn channel_ids(texts: &[String]) -> Result<Vec<ChannelId>, Problem> {
    texts.iter().map(|text| channel_id(text)).collect()
}

fn optional_role(text: Option<&str>) -> Result<Option<RoleId>, Problem> {
    match text {
        Some(text) => snowflake(text)
            .and_then(RoleId::new_checked)
            .map(Some)
            .ok_or(Problem::Key("problem-antiraid-id")),
        None => Ok(None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Setter {
    /// A rule, or the job that ends a timed level.
    Bot,
    User {
        id: u64,
    },
}

/// The level the server is at. A change writes it before it makes the Discord changes its level asks
/// for, and writes it again once they are all made: see [`Level::incomplete`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Level {
    pub name: String,
    pub set_at: i64,
    pub set_by: Setter,
    pub until: Option<i64>,
    /// Counts the changes of the level. A job made for a change acts only while it is the current one.
    #[serde(default)]
    pub generation: u64,
    #[serde(default = "applied_by_default")]
    pub applied: bool,
    #[serde(default)]
    pub gap: Option<Gap>,
    /// The calm level, which a timed level returns to. Kept here so that an expiry needs no section.
    #[serde(default)]
    pub calm: Option<String>,
    #[serde(default)]
    pub tries: u32,
}

/// A record written before the flag existed was written once its effects were made.
fn applied_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gap {
    /// The change was started but not confirmed complete; the process may have stopped in it.
    Unfinished,
    Failed,
    Expiry,
}

impl Level {
    #[must_use]
    pub fn incomplete(&self) -> Option<Gap> {
        (!self.applied).then(|| self.gap.unwrap_or(Gap::Unfinished))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub changed: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub old: String,
    pub new: String,
    pub set_by: Setter,
    pub until: Option<i64>,
    pub tally: Tally,
    pub invites_paused: bool,
    pub gap: Option<Gap>,
    /// Whether the quarantine role of the level is ready for members; `None` without a quarantine.
    pub quarantine_ready: Option<bool>,
}

impl Change {
    /// Whether the change only made the effects of the level that was already set.
    #[must_use]
    pub fn reapplied(&self) -> bool {
        self.old == self.new
    }
}

/// The level, and the revision it was read at (`None` while it was never set, so the calm one).
pub async fn level(store: &Store, calm: &str) -> Result<(Level, Option<i64>), ModuleError> {
    match store
        .entry(LEVEL_KEY)
        .await
        .map_err(|error| store_error(&error))?
    {
        Some(entry) => {
            let level = serde_json::from_value(entry.value).map_err(|error| to_error(&error))?;
            Ok((level, Some(entry.revision)))
        }
        None => Ok((
            Level {
                name: calm.to_owned(),
                set_at: 0,
                set_by: Setter::Bot,
                until: None,
                generation: 0,
                applied: true,
                gap: None,
                calm: Some(calm.to_owned()),
                tries: 0,
            },
            None,
        )),
    }
}

/// The lock of a guild. The level changes of a guild, and the jobs that finish them, take it, so
/// that their effects never interleave.
fn guild_lock(guild: GuildId) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<GuildId, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        locks
            .entry(guild)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
    )
}

/// Changes the level to `name`, and makes the Discord changes that the new level asks for and the
/// old one leaves behind. A timed level is given `duration`, after which the calm level comes back.
/// Returns `None` when the server is at `name` with its effects in place, or when `expect_generation`
/// names another change. Setting the level of a change that is incomplete makes its effects again
/// ([`Change::reapplied`]). A failing effect is counted in the [`Tally`] and the others go on; only a
/// level that cannot be written is an error.
pub async fn set_level(
    ctx: &EventContext,
    cfg: &Antiraid,
    name: &str,
    set_by: Setter,
    duration: Option<Duration>,
    expect_generation: Option<u64>,
    dry_run: bool,
) -> Result<Option<Change>, ModuleError> {
    if !cfg.levels.iter().any(|level| level == name) {
        return Err(ModuleError::other(format!(
            "there is no antiraid level named `{name}`"
        )));
    }
    let lock = guild_lock(ctx.guild_id);
    let _guard = lock.lock().await;
    change(ctx, cfg, name, set_by, duration, expect_generation, dry_run).await
}

/// The change of [`set_level`], made under the lock of the guild.
async fn change(
    ctx: &EventContext,
    cfg: &Antiraid,
    name: &str,
    set_by: Setter,
    duration: Option<Duration>,
    expect_generation: Option<u64>,
    dry_run: bool,
) -> Result<Option<Change>, ModuleError> {
    let store = ctx.store();
    let (old, revision) = level(store, cfg.calm()).await?;
    if expect_generation.is_some_and(|expected| expected != old.generation) {
        return Ok(None);
    }
    let reapply = old.name == name;
    if reapply && old.applied {
        return Ok(None);
    }
    let level = if reapply {
        Level {
            applied: false,
            gap: Some(Gap::Unfinished),
            tries: 0,
            ..old.clone()
        }
    } else {
        let now = ctx.now();
        Level {
            name: name.to_owned(),
            set_at: now,
            set_by,
            until: duration.map(|duration| {
                now.saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
            }),
            generation: old.generation.saturating_add(1),
            applied: false,
            gap: Some(Gap::Unfinished),
            calm: Some(cfg.calm().to_owned()),
            tries: 0,
        }
    };
    // The record goes first: a stop during the Discord changes leaves the level incomplete, which
    // the resume job or setting the same level again finishes. The job is booked before the changes,
    // so that a stop cannot leave the level without one; it does nothing once the level is applied.
    let revision = write_level(store, &level, revision).await?;
    let booked = schedule_resume(ctx, level.generation).await;
    let effect = cfg.effects.get(name).cloned().unwrap_or_default();
    let mut tally = Tally::default();
    let effects = apply_effects(ctx, &effect, &level, &mut tally, dry_run).await;
    let gap = settle(store, &level, revision, gap_of(&effects, &tally)).await;
    if gap.is_some() && booked.is_err() {
        tally.failed += 1;
    }
    let change = Change {
        old: old.name,
        new: name.to_owned(),
        set_by: level.set_by,
        until: level.until,
        tally,
        invites_paused: effects.invites_paused,
        gap,
        quarantine_ready: effects.quarantine_ready,
    };
    notify(ctx, &change, effect.notify).await;
    Ok(Some(change))
}

/// Writes the level if it is still at `revision` (`None`: never written), and returns its revision.
async fn write_level(
    store: &Store,
    level: &Level,
    revision: Option<i64>,
) -> Result<i64, ModuleError> {
    let expect = revision.map_or(Expect::Missing, Expect::Revision);
    match store.replace(LEVEL_KEY, level, expect, None).await {
        Ok(revision) => Ok(revision),
        Err(StorageError::Conflict) => Err(ModuleError::other(
            "the antiraid level was changed at the same time, try again",
        )),
        Err(error) => Err(store_error(&error)),
    }
}

/// Records whether the effects of `level` are all in place, if the record is still at `revision`.
/// A record that cannot be written keeps the state it was written in, which is incomplete, so the
/// gap is then a failure.
async fn settle(store: &Store, level: &Level, revision: i64, gap: Option<Gap>) -> Option<Gap> {
    let settled = Level {
        applied: gap.is_none(),
        gap,
        ..level.clone()
    };
    match write_level(store, &settled, Some(revision)).await {
        Ok(_) => gap,
        Err(_) => Some(gap.unwrap_or(Gap::Failed)),
    }
}

/// Why the effects of a level are not all in place, if they are not.
fn gap_of(effects: &Effects, tally: &Tally) -> Option<Gap> {
    if effects.expiry_failed {
        Some(Gap::Expiry)
    } else if tally.failed > 0 {
        Some(Gap::Failed)
    } else {
        None
    }
}

/// Schedules the next try of a change whose effects are not all in place.
async fn schedule_resume(ctx: &EventContext, generation: u64) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(RESUME_JOB, &Resume { generation })
        .map_err(|error| ModuleError::other(error.to_string()))?
        .key(RESUME_KEY)
        .after(RESUME_DELAY)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))
}

/// The payload of the expiry job: the change it ends, by its generation. A job written before
/// generations existed reads as generation 0, the generation of a level written before them.
#[derive(Debug, Serialize, Deserialize)]
struct Expiry {
    #[serde(default)]
    generation: u64,
}

/// The payload of the resume job: the change whose effects it applies again.
#[derive(Debug, Serialize, Deserialize)]
struct Resume {
    generation: u64,
}

/// The invites marker: that antiraid paused the invites, and by which change. It is written before a
/// pause is made, and cleared only after a resume is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct InvitesMark {
    state: InvitesState,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InvitesState {
    Pausing,
    Paused,
    Resuming,
}

/// The marker as stored: a bare `true` is what the module stored before the marker had a state.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StoredMark {
    Current(InvitesMark),
    Legacy(bool),
}

/// What applying the effects of a level left to report.
struct Effects {
    invites_paused: bool,
    quarantine_ready: Option<bool>,
    expiry_failed: bool,
}

/// Makes the Discord changes and the schedule that `effect` and `level` ask for. Each step is tried
/// whatever the others did; failures are counted in `tally`.
async fn apply_effects(
    ctx: &EventContext,
    effect: &Effect,
    level: &Level,
    tally: &mut Tally,
    dry_run: bool,
) -> Effects {
    slowmodes(ctx, effect.slowmode.as_ref(), tally).await;
    let invites_paused = invites(ctx, effect.pause_invites, level.generation, tally).await;
    // A dry run sets up no role, so the notice has no readiness to report.
    let quarantine_ready = match effect.quarantine {
        Some(quarantine) if !dry_run => Some(quarantine_role_ready(ctx, quarantine, tally).await),
        _ => None,
    };
    let expiry_failed = !schedule(ctx, level, tally).await;
    Effects {
        invites_paused,
        quarantine_ready,
        expiry_failed,
    }
}

/// Starts the sync of the quarantine role that the level needs, and says whether the role is ready
/// for members: a configured role is, a created one once its channels are set up.
async fn quarantine_role_ready(
    ctx: &EventContext,
    quarantine: Quarantine,
    tally: &mut Tally,
) -> bool {
    let Ok(role) = ensure_role(ctx, RestrictionRole::Quarantine, quarantine.role).await else {
        tally.failed += 1;
        return false;
    };
    quarantine.role == Some(role) || synced_to(ctx.store(), role).await
}

/// Whether the channel sync of the quarantine role has finished for `role`.
async fn synced_to(store: &Store, role: RoleId) -> bool {
    store.get::<u64>(QUARANTINE_SYNCED_KEY).await.ok().flatten() == Some(role.get())
}

/// Whether the quarantine role of a level is ready for members, as the view and the notice say.
pub async fn quarantine_is_ready(store: &Store, quarantine: Quarantine) -> bool {
    quarantine.role.is_some()
        || store
            .get::<u64>(QUARANTINE_SYNCED_KEY)
            .await
            .ok()
            .flatten()
            .is_some()
}

/// Schedules the end of a timed level, or cancels the end of an untimed one. Returns whether that
/// worked; a failure is counted in `tally`.
async fn schedule(ctx: &EventContext, level: &Level, tally: &mut Tally) -> bool {
    let scheduler = ctx.scheduler();
    let result: Result<(), String> = match level.until {
        Some(until) => {
            let delay =
                Duration::from_secs(u64::try_from(until.saturating_sub(ctx.now())).unwrap_or(0));
            match scheduler.job(
                EXPIRE_JOB,
                &Expiry {
                    generation: level.generation,
                },
            ) {
                Ok(job) => job
                    .key(EXPIRE_KEY)
                    .after(delay)
                    .await
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            }
        }
        None => scheduler
            .cancel(EXPIRE_KEY)
            .await
            .map(drop)
            .map_err(|error| error.to_string()),
    };
    if result.is_err() {
        tally.failed += 1;
    }
    result.is_ok()
}

/// Applies the end of a timed level. The calm level comes back when the change the job was made for
/// is still the current one; a change made since is left alone. Without an antiraid section the
/// stored changes are reverted instead.
pub async fn expire(state: &State, cx: JobCx) -> Result<(), ModuleError> {
    let payload: Expiry = cx.payload()?;
    let loaded = state.load(cx.ctx())?;
    let Some(cfg) = loaded.setup.antiraid.as_ref() else {
        return revert_unconfigured(state, cx.ctx(), payload.generation).await;
    };
    let change = set_level(
        cx.ctx(),
        cfg,
        cfg.calm(),
        Setter::Bot,
        None,
        Some(payload.generation),
        loaded.config.dry_run,
    )
    .await?;
    if let Some(change) = change {
        engine::after_command(
            state,
            cx.ctx(),
            vec![Context::antiraid(&change.new, &change.old)],
        )
        .await?;
    }
    Ok(())
}

/// The end of a timed level when the antiraid section is gone: the stored changes are reverted
/// without it. The slowmodes go back to their stored originals and the invites open if antiraid
/// paused them; the level goes to the calm level it was written with.
async fn revert_unconfigured(
    state: &State,
    ctx: &EventContext,
    generation: u64,
) -> Result<(), ModuleError> {
    let lock = guild_lock(ctx.guild_id);
    let change = {
        let _guard = lock.lock().await;
        revert_locked(ctx, generation).await?
    };
    if let Some(change) = change {
        engine::after_command(
            state,
            ctx,
            vec![Context::antiraid(&change.new, &change.old)],
        )
        .await?;
    }
    Ok(())
}

/// The revert of [`revert_unconfigured`], made under the lock of the guild.
async fn revert_locked(ctx: &EventContext, generation: u64) -> Result<Option<Change>, ModuleError> {
    let store = ctx.store();
    let (old, revision) = level(store, DEFAULT_LEVELS[0]).await?;
    if old.generation != generation || old.until.is_none() {
        return Ok(None);
    }
    let calm = old
        .calm
        .clone()
        .unwrap_or_else(|| DEFAULT_LEVELS[0].to_owned());
    let level = Level {
        name: calm.clone(),
        set_at: ctx.now(),
        set_by: Setter::Bot,
        until: None,
        generation: old.generation.saturating_add(1),
        applied: false,
        gap: Some(Gap::Unfinished),
        calm: Some(calm.clone()),
        tries: 0,
    };
    // As for a level that is set: the record goes first and the resume is booked before the
    // changes are put back, so that a stop in between cannot leave them without a retry. The
    // section is gone, but the changes it made are still there; the job does nothing once the
    // level is applied.
    let revision = write_level(store, &level, revision).await?;
    let booked = schedule_resume(ctx, level.generation).await;
    let mut tally = Tally::default();
    slowmodes(ctx, None, &mut tally).await;
    let invites_paused = invites(ctx, false, generation, &mut tally).await;
    let gap = settle(
        store,
        &level,
        revision,
        (tally.failed > 0).then_some(Gap::Failed),
    )
    .await;
    if gap.is_some() && booked.is_err() {
        tally.failed += 1;
    }
    let change = Change {
        old: old.name,
        new: calm,
        set_by: Setter::Bot,
        until: None,
        tally,
        invites_paused,
        gap,
        quarantine_ready: None,
    };
    notify(ctx, &change, None).await;
    if let Some(log) = ctx.policy().log_channel {
        send(ctx, log, text(ctx, "antiraid-unconfigured", &Args::new())).await;
    }
    Ok(Some(change))
}

/// Applies again the effects of the current level when its change is the one the job was made for
/// and its effects are not all in place. A try that does not finish schedules the next one, until
/// [`RESUME_TRIES`].
pub async fn resume(state: &State, cx: JobCx) -> Result<(), ModuleError> {
    let payload: Resume = cx.payload()?;
    let loaded = state.load(cx.ctx())?;
    let ctx = cx.ctx();
    // The effects come from the section: without it there is only the revert to finish.
    let Some(cfg) = loaded.setup.antiraid.as_ref() else {
        return resume_unconfigured(ctx, payload.generation).await;
    };
    let lock = guild_lock(ctx.guild_id);
    let _guard = lock.lock().await;
    let store = ctx.store();
    let (current, revision) = level(store, cfg.calm()).await?;
    let Some(revision) = revision else {
        return Ok(());
    };
    if current.applied || current.generation != payload.generation || current.tries >= RESUME_TRIES
    {
        return Ok(());
    }
    let effect = cfg.effects.get(&current.name).cloned().unwrap_or_default();
    let mut tally = Tally::default();
    let effects = apply_effects(ctx, &effect, &current, &mut tally, loaded.config.dry_run).await;
    let next = Level {
        tries: current.tries + 1,
        ..current
    };
    let gap = settle(store, &next, revision, gap_of(&effects, &tally)).await;
    if gap.is_some() && next.tries < RESUME_TRIES {
        schedule_resume(ctx, next.generation).await?;
    }
    Ok(())
}

/// Puts back again what the revert of a level without its section could not: the slowmodes and the
/// invites. A try that does not finish schedules the next one, until [`RESUME_TRIES`].
async fn resume_unconfigured(ctx: &EventContext, generation: u64) -> Result<(), ModuleError> {
    let lock = guild_lock(ctx.guild_id);
    let _guard = lock.lock().await;
    let store = ctx.store();
    let (current, revision) = level(store, DEFAULT_LEVELS[0]).await?;
    let Some(revision) = revision else {
        return Ok(());
    };
    if current.applied || current.generation != generation || current.tries >= RESUME_TRIES {
        return Ok(());
    }
    let mut tally = Tally::default();
    slowmodes(ctx, None, &mut tally).await;
    invites(ctx, false, generation, &mut tally).await;
    let next = Level {
        tries: current.tries + 1,
        ..current
    };
    let gap = settle(
        store,
        &next,
        revision,
        (tally.failed > 0).then_some(Gap::Failed),
    )
    .await;
    if gap.is_some() && next.tries < RESUME_TRIES {
        schedule_resume(ctx, next.generation).await?;
    }
    Ok(())
}

/// Sets the slowmode the level wants on its channels, remembering each original first, and restores
/// the originals of the channels that the level no longer covers.
async fn slowmodes(ctx: &EventContext, wanted: Option<&Slowmode>, tally: &mut Tally) {
    let store = ctx.store();
    let Ok(remembered) = remembered_slowmodes(store).await else {
        tally.failed += 1;
        return;
    };
    if wanted.is_none() && remembered.is_empty() {
        return;
    }
    let Ok(channels) = text_channels(ctx).await else {
        tally.failed += 1;
        return;
    };
    let targets = wanted.map_or_else(BTreeMap::new, |wanted| targets(wanted, &channels));
    for (channel, original) in remembered
        .iter()
        .filter(|(channel, _)| !targets.contains_key(channel))
    {
        match edit_slowmode(ctx, *channel, *original).await {
            Ok(()) => {
                tally.changed += 1;
                forget_slowmode(store, *channel).await;
            }
            // A channel that is gone has no slowmode left to restore.
            Err(DiscordError::NotFound) => forget_slowmode(store, *channel).await,
            Err(_) => tally.failed += 1,
        }
    }
    for (channel, seconds) in &targets {
        if !remembered.iter().any(|(known, _)| known == channel) {
            let current = rate_limit_of(&channels, *channel);
            // Only the first original is kept: a stored one is never replaced.
            match store
                .replace(&slowmode_key(*channel), &current, Expect::Missing, None)
                .await
            {
                Ok(_) | Err(StorageError::Conflict) => {}
                Err(_) => {
                    tally.failed += 1;
                    continue;
                }
            }
        }
        if rate_limit_of(&channels, *channel) == *seconds {
            continue;
        }
        match edit_slowmode(ctx, *channel, *seconds).await {
            Ok(()) => tally.changed += 1,
            Err(_) => tally.failed += 1,
        }
    }
}

/// The channels a slowmode reaches, of those that exist.
fn targets(wanted: &Slowmode, channels: &[Channel]) -> BTreeMap<ChannelId, u16> {
    let ids: Vec<ChannelId> = if wanted.channels.is_empty() {
        channels
            .iter()
            .take(MAX_CHANNELS)
            .map(|channel| channel.id)
            .collect()
    } else {
        wanted.channels.clone()
    };
    ids.into_iter()
        .filter(|id| channels.iter().any(|channel| channel.id == *id))
        .map(|id| (id, wanted.seconds))
        .collect()
}

fn rate_limit_of(channels: &[Channel], channel: ChannelId) -> u16 {
    channels
        .iter()
        .find(|known| known.id == channel)
        .and_then(|known| known.rate_limit_per_user)
        .unwrap_or(0)
}

/// The text channels of the guild, lowest id first.
async fn text_channels(ctx: &EventContext) -> Result<Vec<Channel>, DiscordError> {
    match ctx
        .api
        .execute(Action::ListChannels {
            guild_id: ctx.guild_id,
        })
        .await?
    {
        Outcome::Channels(mut channels) => {
            channels.retain(|channel| channel.kind == ChannelType::GuildText);
            channels.sort_unstable_by_key(|channel| channel.id.get());
            Ok(channels)
        }
        _ => Err(DiscordError::Fatal(
            "unexpected outcome for `list_channels`".into(),
        )),
    }
}

async fn edit_slowmode(
    ctx: &EventContext,
    channel: ChannelId,
    seconds: u16,
) -> Result<(), DiscordError> {
    let reason = Reason::truncated(&text(ctx, "antiraid-reason", &Args::new()));
    ctx.api
        .execute(Action::EditChannel {
            channel_id: channel,
            patch: ChannelPatch {
                slowmode_seconds: Some(seconds),
                ..ChannelPatch::default()
            },
            reason,
        })
        .await
        .map(drop)
}

fn slowmode_key(channel: ChannelId) -> String {
    format!("{SLOWMODE_KEY}{}", channel.get())
}

async fn forget_slowmode(store: &Store, channel: ChannelId) {
    // A key that stays only makes a later change restore the same original again.
    let _ = store.delete(&slowmode_key(channel)).await;
}

/// The channels whose original slowmode is remembered, with that original.
async fn remembered_slowmodes(store: &Store) -> Result<Vec<(ChannelId, u16)>, ModuleError> {
    let entries = store
        .list(SLOWMODE_KEY, LIST_LIMIT)
        .await
        .map_err(|error| store_error(&error))?;
    Ok(entries
        .into_iter()
        .filter_map(|(key, entry)| {
            let channel = key
                .strip_prefix(SLOWMODE_KEY)?
                .parse::<u64>()
                .ok()
                .and_then(ChannelId::new_checked)?;
            let seconds = serde_json::from_value::<u16>(entry.value).ok()?;
            Some((channel, seconds))
        })
        .collect())
}

/// Pauses or resumes the invites when the level asks for another state than they are in. A pause is
/// recorded before it is made, and only a recorded pause is resumed; invites that the server closed
/// itself are left alone. Returns whether the invites are paused by antiraid once this is done.
async fn invites(ctx: &EventContext, pause: bool, generation: u64, tally: &mut Tally) -> bool {
    let store = ctx.store();
    let Ok(mark) = stored_mark(store).await else {
        tally.failed += 1;
        return false;
    };
    if pause {
        if mark.is_some_and(|mark| mark.state == InvitesState::Paused) {
            return true;
        }
        if mark.is_none() {
            match invites_disabled(ctx).await {
                Ok(false) => {}
                Ok(true) => return false,
                Err(_) => {
                    tally.failed += 1;
                    return false;
                }
            }
        }
        if !write_mark(store, InvitesState::Pausing, generation).await {
            tally.failed += 1;
            return false;
        }
        if !edit_invites(ctx, true, tally).await {
            return false;
        }
        if !write_mark(store, InvitesState::Paused, generation).await {
            tally.failed += 1;
        }
        return true;
    }
    if mark.is_none() {
        return false;
    }
    let Ok(disabled) = invites_disabled(ctx).await else {
        tally.failed += 1;
        return true;
    };
    if disabled {
        if !write_mark(store, InvitesState::Resuming, generation).await {
            tally.failed += 1;
            return true;
        }
        if !edit_invites(ctx, false, tally).await {
            return true;
        }
    }
    // The mark goes only after the change: when it stays, the resume job finishes the change.
    if store.delete(INVITES_KEY).await.is_err() {
        tally.failed += 1;
    }
    false
}

/// The invites marker, if antiraid paused the invites.
async fn stored_mark(store: &Store) -> Result<Option<InvitesMark>, StorageError> {
    Ok(match store.get::<StoredMark>(INVITES_KEY).await? {
        Some(StoredMark::Current(mark)) => Some(mark),
        Some(StoredMark::Legacy(true)) => Some(InvitesMark {
            state: InvitesState::Paused,
            generation: 0,
        }),
        Some(StoredMark::Legacy(false)) | None => None,
    })
}

/// Writes the invites marker; whether that worked.
async fn write_mark(store: &Store, state: InvitesState, generation: u64) -> bool {
    store
        .set(INVITES_KEY, &InvitesMark { state, generation })
        .await
        .is_ok()
}

/// Switches the invites of the guild off or on; whether Discord made the change.
async fn edit_invites(ctx: &EventContext, disabled: bool, tally: &mut Tally) -> bool {
    let reason = Reason::truncated(&text(ctx, "antiraid-reason", &Args::new()));
    let patch = GuildPatch {
        invites_disabled: Some(disabled),
        ..GuildPatch::default()
    };
    if ctx
        .api
        .execute(Action::EditGuild {
            guild_id: ctx.guild_id,
            patch,
            reason,
        })
        .await
        .is_err()
    {
        tally.failed += 1;
        return false;
    }
    tally.changed += 1;
    true
}

/// Whether the invites of the guild are switched off, by antiraid or by the server.
async fn invites_disabled(ctx: &EventContext) -> Result<bool, DiscordError> {
    match ctx
        .api
        .execute(Action::GetGuild {
            guild_id: ctx.guild_id,
        })
        .await?
    {
        Outcome::Guild(guild) => Ok(guild
            .features
            .iter()
            .any(|feature| matches!(feature, GuildFeature::InvitesDisabled))),
        _ => Ok(false),
    }
}

/// The text that follows a sentence in a notice: a space before it, nothing when there is none.
fn following(sentence: String) -> String {
    if sentence.is_empty() {
        sentence
    } else {
        format!(" {sentence}")
    }
}

/// The locale key of the reason a level is incomplete.
pub fn gap_key(gap: Gap) -> &'static str {
    match gap {
        Gap::Unfinished => "antiraid-gap-unfinished",
        Gap::Failed => "antiraid-gap-failed",
        Gap::Expiry => "antiraid-gap-expiry",
    }
}

/// Posts `body` to `channel`. A notice that does not arrive does not undo the change it tells of.
async fn send(ctx: &EventContext, channel: ChannelId, body: String) {
    // The description is cut to fit, so the embed always builds.
    let Ok(embed) = EmbedBuilder::new()
        .description(truncate(&body, DESCRIPTION_LIMIT))
        .color(Color::Warning)
        .build()
    else {
        return;
    };
    let _ = ctx.api.send_embeds(channel, vec![embed]).await;
}

/// Tells the log channel, and the notify channel of the level when it has one, what changed.
async fn notify(ctx: &EventContext, change: &Change, notify: Option<ChannelId>) {
    let who = match change.set_by {
        Setter::Bot => text(ctx, "antiraid-by-rule", &Args::new()),
        Setter::User { id } => format!("<@{id}>"),
    };
    let until = change.until.map_or_else(String::new, |at| {
        text(ctx, "antiraid-until", &Args::new().with("at", at))
    });
    let invites_key = if change.invites_paused {
        "antiraid-invites-paused"
    } else {
        "antiraid-invites-open"
    };
    let quarantine = match change.quarantine_ready {
        Some(true) => text(ctx, "antiraid-quarantine-ready", &Args::new()),
        Some(false) => text(ctx, "antiraid-quarantine-syncing", &Args::new()),
        None => String::new(),
    };
    let incomplete = change.gap.map_or_else(String::new, |gap| {
        text(
            ctx,
            "antiraid-incomplete",
            &Args::new().with("reason", text(ctx, gap_key(gap), &Args::new())),
        )
    });
    let body = text(
        ctx,
        "antiraid-notice",
        &Args::new()
            .with("old", change.old.as_str())
            .with("new", change.new.as_str())
            .with("who", who)
            .with("until", until)
            .with("changed", count(change.tally.changed))
            .with("failed", count(change.tally.failed))
            .with("invites", text(ctx, invites_key, &Args::new()))
            .with("quarantine", following(quarantine))
            .with("incomplete", following(incomplete)),
    );
    let mut channels = Vec::new();
    if let Some(log) = ctx.policy().log_channel {
        channels.push(log);
    }
    if let Some(channel) = notify
        && !channels.contains(&channel)
    {
        channels.push(channel);
    }
    for channel in channels {
        send(ctx, channel, body.clone()).await;
    }
}

/// A quarantined member, with the roles given to it (`marker` is zero when there is none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    user: u64,
    role: u64,
    marker: u64,
}

fn chunk_key(index: usize) -> String {
    format!("{QUARANTINE_KEY}{index}")
}

async fn chunk(store: &Store, index: usize) -> Result<(Vec<Held>, Expect), ModuleError> {
    match store
        .entry(&chunk_key(index))
        .await
        .map_err(|error| store_error(&error))?
    {
        Some(entry) => Ok((
            serde_json::from_value(entry.value).map_err(|error| to_error(&error))?,
            Expect::Revision(entry.revision),
        )),
        None => Ok((Vec::new(), Expect::Missing)),
    }
}

/// What a pair of roles (the quarantine role and the marker) holds is a set of members of the
/// store, so a quarantine of any size costs a row for each member, and joining, leaving and looking
/// a member up cost the same however many are held. The pairs are a short list: a pair is added
/// when a level names new roles, and its place in the list is the name of its set.
const PAIRS_KEY: &str = "antiraid:pairs";
const MIGRATED_KEY: &str = "antiraid:ledger";
/// The most pairs of roles the levels may use over the life of the server.
const MAX_PAIRS: usize = 64;
/// How many members one step of a release takes from a set.
const RELEASE_BATCH: usize = 100;

fn set_of(index: usize) -> String {
    format!("antiraid-held-{index}")
}

async fn pairs(store: &Store) -> Result<(Vec<[u64; 2]>, Expect), ModuleError> {
    match store
        .entry(PAIRS_KEY)
        .await
        .map_err(|error| store_error(&error))?
    {
        Some(entry) => Ok((
            serde_json::from_value(entry.value).map_err(|error| to_error(&error))?,
            Expect::Revision(entry.revision),
        )),
        None => Ok((Vec::new(), Expect::Missing)),
    }
}

/// The place of a pair of roles in the list, added when it is new. The list is written against the
/// revision it was read at, so two joins with a new pair do not lose each other.
async fn pair_index(store: &Store, role: u64, marker: u64) -> Result<usize, ModuleError> {
    for _ in 0..CAS_TRIES {
        let (mut all, expect) = pairs(store).await?;
        if let Some(index) = all.iter().position(|pair| *pair == [role, marker]) {
            return Ok(index);
        }
        if all.len() >= MAX_PAIRS {
            return Err(full());
        }
        all.push([role, marker]);
        match store.replace(PAIRS_KEY, &all, expect, None).await {
            Ok(_) => return Ok(all.len() - 1),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(store_error(&error)),
        }
    }
    Err(busy())
}

/// Moves a ledger written as chunks (the first format) into the sets, once.
async fn migrate(store: &Store) -> Result<(), ModuleError> {
    if store
        .get::<bool>(MIGRATED_KEY)
        .await
        .map_err(|error| store_error(&error))?
        .is_some()
    {
        return Ok(());
    }
    for index in 0..LEGACY_CHUNKS {
        let (members, _) = chunk(store, index).await?;
        for held in &members {
            hold(store, *held).await?;
        }
        if !members.is_empty() {
            store
                .delete(&chunk_key(index))
                .await
                .map_err(|error| store_error(&error))?;
        }
    }
    // Only a shortcut for the next call: a full store that cannot take it just reads the chunks again.
    let _ = store.set(MIGRATED_KEY, &true).await;
    Ok(())
}

/// The pairs a member is held under.
async fn held_for(store: &Store, user: u64) -> Result<Vec<Held>, ModuleError> {
    let mut found = Vec::new();
    for (index, [role, marker]) in pairs(store).await?.0.into_iter().enumerate() {
        if store
            .set_has(&set_of(index), user)
            .await
            .map_err(|error| store_error(&error))?
        {
            found.push(Held { user, role, marker });
        }
    }
    Ok(found)
}

/// Records a quarantined member under the pair of roles it was given.
async fn hold(store: &Store, held: Held) -> Result<(), ModuleError> {
    let index = pair_index(store, held.role, held.marker).await?;
    store
        .set_add(&set_of(index), held.user)
        .await
        .map(drop)
        .map_err(|error| store_error(&error))
}

fn full() -> ModuleError {
    ModuleError::other("the levels have named too many pairs of quarantine roles")
}

fn busy() -> ModuleError {
    ModuleError::other("the antiraid quarantine changed at the same time, try again")
}

/// Forgets a member, wherever it is recorded.
async fn forget(store: &Store, user: u64) -> Result<(), ModuleError> {
    for index in 0..pairs(store).await?.0.len() {
        store
            .set_remove(&set_of(index), user)
            .await
            .map_err(|error| store_error(&error))?;
    }
    Ok(())
}

/// Gives the quarantine role (and the marker role) to a member who joins at a level that has one.
/// The member is recorded before the quarantine role is given, so a role that was given is always
/// recorded. A quarantine role that cannot be given takes a new record back; a marker role that
/// cannot be given is reported and keeps the record, so that release still takes the quarantine off.
async fn quarantine_join(
    ctx: &EventContext,
    memory: &Memory,
    cfg: &Antiraid,
    user: UserId,
) -> Result<(), ModuleError> {
    // A member's hold, give and release do not interleave: a release between the hold and the give
    // would leave a role that no record names.
    let _turn = member_turn(ctx, user).await;
    let store = ctx.store();
    migrate(store).await?;
    let (current, _) = level(store, cfg.calm()).await?;
    let Some(quarantine) = cfg
        .effects
        .get(&current.name)
        .and_then(|effect| effect.quarantine)
    else {
        return Ok(());
    };
    let role = ensure_role(ctx, RestrictionRole::Quarantine, quarantine.role)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let held = held_for(store, user.get()).await?;
    let fresh = held.is_empty();
    if !held.iter().any(|entry| {
        entry.role == role.get() && entry.marker == quarantine.marker.map_or(0, RoleId::get)
    }) {
        hold(
            store,
            Held {
                user: user.get(),
                role: role.get(),
                marker: quarantine.marker.map_or(0, RoleId::get),
            },
        )
        .await?;
    }
    if let Err(error) = give(ctx, memory, user, role).await {
        if fresh {
            forget(store, user.get()).await?;
        }
        return Err(error);
    }
    match quarantine.marker {
        Some(marker) => give(ctx, memory, user, marker).await,
        None => Ok(()),
    }
}

async fn give(
    ctx: &EventContext,
    memory: &Memory,
    user: UserId,
    role: RoleId,
) -> Result<(), ModuleError> {
    memory.ignore_role(ctx.guild_id, user, role, ctx.now());
    let reason = Reason::truncated(&text(ctx, "antiraid-reason", &Args::new()));
    ctx.api
        .add_role(ctx.guild_id, user, role, reason)
        .await
        .map_err(ModuleError::from)
}

/// Takes the roles of one pair off a member; whether both are off.
async fn take_off(
    ctx: &EventContext,
    memory: &Memory,
    user: UserId,
    [role, marker]: [u64; 2],
    tally: &mut Tally,
) -> bool {
    let mut all_taken = true;
    for role in [role, marker].into_iter().filter_map(RoleId::new_checked) {
        memory.ignore_role(ctx.guild_id, user, role, ctx.now());
        let reason = Reason::truncated(&text(ctx, "antiraid-reason", &Args::new()));
        match ctx.api.remove_role(ctx.guild_id, user, role, reason).await {
            Ok(()) | Err(DiscordError::NotFound) => {}
            Err(_) => {
                all_taken = false;
                tally.failed += 1;
            }
        }
    }
    all_taken
}

/// Takes the quarantine role and the marker role off a member, and forgets it once both are off.
pub async fn release(
    ctx: &EventContext,
    memory: &Memory,
    user: UserId,
) -> Result<Tally, ModuleError> {
    let _turn = member_turn(ctx, user).await;
    let store = ctx.store();
    migrate(store).await?;
    let mut tally = Tally::default();
    let all = pairs(store).await?.0;
    for (index, pair) in all.into_iter().enumerate() {
        if !store
            .set_has(&set_of(index), user.get())
            .await
            .map_err(|error| store_error(&error))?
        {
            continue;
        }
        if take_off(ctx, memory, user, pair, &mut tally).await {
            store
                .set_remove(&set_of(index), user.get())
                .await
                .map_err(|error| store_error(&error))?;
            tally.changed += 1;
        }
    }
    Ok(tally)
}

/// Releases every quarantined member, a batch of each set at a time. A member leaves the set only
/// once the role is back, so a stop in the middle leaves the rest quarantined and in their sets for
/// the next release; one that could not be released stays there and the others go on.
pub async fn release_all(ctx: &EventContext, memory: &Memory) -> Result<Tally, ModuleError> {
    let store = ctx.store();
    migrate(store).await?;
    let mut total = Tally::default();
    for (index, pair) in pairs(store).await?.0.into_iter().enumerate() {
        let mut after = 0_u64;
        loop {
            let batch = store
                .set_after(&set_of(index), after, RELEASE_BATCH)
                .await
                .map_err(|error| store_error(&error))?;
            let Some(last) = batch.last().copied() else {
                break;
            };
            after = last;
            for user in batch.into_iter().filter_map(UserId::new_checked) {
                let _turn = member_turn(ctx, user).await;
                if take_off(ctx, memory, user, pair, &mut total).await {
                    store
                        .set_remove(&set_of(index), user.get())
                        .await
                        .map_err(|error| store_error(&error))?;
                    total.changed += 1;
                }
            }
        }
    }
    Ok(total)
}

/// Quarantines a member who joins, and forgets one who leaves. Bots are never quarantined.
pub async fn member_event(
    ctx: &EventContext,
    memory: &Memory,
    cfg: Option<&Antiraid>,
    event: &Event,
    dry_run: bool,
) -> Result<(), ModuleError> {
    let Some(cfg) = cfg else {
        return Ok(());
    };
    match event {
        Event::MemberJoined(info) if !info.user.bot && !dry_run => {
            quarantine_join(ctx, memory, cfg, info.user.id).await
        }
        Event::MemberLeft { user, .. } => {
            migrate(ctx.store()).await?;
            forget(ctx.store(), user.id.get()).await
        }
        _ => Ok(()),
    }
}

/// How many members are quarantined now.
pub async fn quarantined_count(store: &Store) -> Result<usize, ModuleError> {
    migrate(store).await?;
    let mut total = 0;
    for index in 0..pairs(store).await?.0.len() {
        total += store
            .set_count(&set_of(index))
            .await
            .map_err(|error| store_error(&error))?;
    }
    Ok(usize::try_from(total).unwrap_or(usize::MAX))
}

fn count(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn store_error(error: &StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn to_error(error: &serde_json::Error) -> ModuleError {
    ModuleError::other(error.to_string())
}
