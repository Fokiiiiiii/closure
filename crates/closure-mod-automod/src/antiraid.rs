//! Antiraid: the levels a server raises while a raid runs, what each level changes, and the
//! quarantine of the members who join at a level that has one.
//!
//! The level is one record in the store, and [`set_level`] is the only place it changes: the
//! command, a rule's action and the job that ends a timed level all use it. Each change is applied
//! by comparing with what the module changed before, so that a later level restores what an earlier
//! one set (a slowmode) or paused (the invites).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use closure_core::action::{Action, ChannelPatch, GuildPatch, MentionPolicy, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_sdk::punish::{RestrictionRole, ensure_role};
use closure_sdk::{
    Args, ChannelId, Event, EventContext, Expect, JobCx, ModuleError, RoleId, StorageError, Store,
    UserId,
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
/// How many members are quarantined at once; a join past this is not quarantined.
const MAX_QUARANTINED: usize = 5_000;
/// The quarantined members are kept in chunks, each well inside one store value.
const CHUNK_MEMBERS: usize = 300;
const CHUNKS: usize = MAX_QUARANTINED.div_ceil(CHUNK_MEMBERS);
/// How often a write that lost a race to another write is tried again.
const CAS_TRIES: usize = 5;
/// The job that returns a timed level to the calm one.
pub const EXPIRE_JOB: &str = "antiraid_expire";
const EXPIRE_KEY: &str = "antiraid";
const LEVEL_KEY: &str = "antiraid:level";
const INVITES_KEY: &str = "antiraid:invites";
const SLOWMODE_KEY: &str = "antiraid:slowmode:";
const QUARANTINE_KEY: &str = "antiraid:quarantined:";
/// The most entries one listing of the store returns.
const LIST_LIMIT: usize = 1_000;

// ---- configuration ----

/// The antiraid section as written. It is read on its own, so that a bad section disables antiraid
/// and nothing else.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AntiraidConfig {
    pub levels: Option<Vec<String>>,
    pub effects: BTreeMap<String, EffectConfig>,
    pub quarantine_release: bool,
}

/// What a level does, as written.
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
    /// The calm level, the one a server is at until it is raised.
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

// ---- the level ----

/// Who raised or lowered the level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Setter {
    /// A rule, or the job that ends a timed level.
    Bot,
    User {
        id: u64,
    },
}

/// The level the server is at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Level {
    pub name: String,
    /// Unix seconds of the change.
    pub set_at: i64,
    pub set_by: Setter,
    /// When a timed level ends, if it does.
    pub until: Option<i64>,
}

/// How many Discord changes a level change made, and how many failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub changed: usize,
    pub failed: usize,
}

/// What a level change did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub old: String,
    pub new: String,
    pub set_by: Setter,
    pub until: Option<i64>,
    pub tally: Tally,
    /// Whether the invites are paused by antiraid once the change is made.
    pub invites_paused: bool,
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
            },
            None,
        )),
    }
}

/// Changes the level to `name`, and makes the Discord changes that the new level asks for and the
/// old one leaves behind. A timed level is given `duration`, after which the calm level comes back.
/// Returns `None` when the server is at `name` already. A failing change is counted in the
/// [`Tally`] and the others go on; only a level that cannot be written is an error.
pub async fn set_level(
    ctx: &EventContext,
    cfg: &Antiraid,
    name: &str,
    set_by: Setter,
    duration: Option<Duration>,
) -> Result<Option<Change>, ModuleError> {
    if !cfg.levels.iter().any(|level| level == name) {
        return Err(ModuleError::other(format!(
            "there is no antiraid level named `{name}`"
        )));
    }
    let store = ctx.store();
    let (old, revision) = level(store, cfg.calm()).await?;
    if old.name == name {
        return Ok(None);
    }
    let now = ctx.now();
    let until = duration
        .map(|duration| now.saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)));
    let new = Level {
        name: name.to_owned(),
        set_at: now,
        set_by,
        until,
    };
    write_level(store, &new, revision).await?;

    let effect = cfg.effects.get(name).cloned().unwrap_or_default();
    let mut tally = Tally::default();
    slowmodes(ctx, effect.slowmode.as_ref(), &mut tally).await;
    let invites_paused = invites(ctx, effect.pause_invites, &mut tally).await;
    schedule(ctx, duration, now, &mut tally).await;
    let change = Change {
        old: old.name,
        new: new.name,
        set_by,
        until,
        tally,
        invites_paused,
    };
    notify(ctx, &change, effect.notify).await;
    Ok(Some(change))
}

async fn write_level(
    store: &Store,
    level: &Level,
    revision: Option<i64>,
) -> Result<(), ModuleError> {
    let expect = revision.map_or(Expect::Missing, Expect::Revision);
    match store.replace(LEVEL_KEY, level, expect, None).await {
        Ok(_) => Ok(()),
        Err(StorageError::Conflict) => Err(ModuleError::other(
            "the antiraid level was changed at the same time, try again",
        )),
        Err(error) => Err(store_error(&error)),
    }
}

/// The payload of the expiry job: the change it ends. A later change has another `set_at`.
#[derive(Debug, Serialize, Deserialize)]
struct Expiry {
    set_at: i64,
}

async fn schedule(ctx: &EventContext, duration: Option<Duration>, now: i64, tally: &mut Tally) {
    let scheduler = ctx.scheduler();
    let result: Result<(), String> = match duration {
        Some(duration) => match scheduler.job(EXPIRE_JOB, &Expiry { set_at: now }) {
            Ok(job) => job
                .key(EXPIRE_KEY)
                .after(duration)
                .await
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        },
        None => scheduler
            .cancel(EXPIRE_KEY)
            .await
            .map(drop)
            .map_err(|error| error.to_string()),
    };
    if result.is_err() {
        tally.failed += 1;
    }
}

/// Applies the expiry of a timed level: the calm level comes back when the change it belongs to is
/// still the current one.
pub async fn expire(state: &State, cx: JobCx) -> Result<(), ModuleError> {
    let payload: Expiry = cx.payload()?;
    let loaded = state.load(cx.ctx())?;
    let Some(cfg) = loaded.setup.antiraid.as_ref() else {
        return Ok(());
    };
    let (current, _) = level(cx.store(), cfg.calm()).await?;
    if current.set_at != payload.set_at || current.until.is_none() {
        return Ok(());
    }
    if let Some(change) = set_level(cx.ctx(), cfg, cfg.calm(), Setter::Bot, None).await? {
        engine::after_command(
            state,
            cx.ctx(),
            vec![Context::antiraid(&change.new, &change.old)],
        )
        .await?;
    }
    Ok(())
}

// ---- slowmodes ----

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
            if store.set(&slowmode_key(*channel), &current).await.is_err() {
                tally.failed += 1;
                continue;
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

/// Pauses or resumes the invites when the level asks for another state than they are in. Invites
/// that the server closed itself are left alone, and so are not resumed later. Returns whether the
/// invites are paused by antiraid once this is done.
async fn invites(ctx: &EventContext, pause: bool, tally: &mut Tally) -> bool {
    let store = ctx.store();
    let Ok(paused) = store.get::<bool>(INVITES_KEY).await else {
        tally.failed += 1;
        return false;
    };
    let paused = paused.unwrap_or(false);
    if pause == paused {
        return paused;
    }
    if pause {
        match closed_by_server(ctx).await {
            Ok(false) => {}
            Ok(true) => return false,
            Err(_) => {
                tally.failed += 1;
                return false;
            }
        }
    }
    let reason = Reason::truncated(&text(ctx, "antiraid-reason", &Args::new()));
    let patch = GuildPatch {
        invites_disabled: Some(pause),
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
        return paused;
    }
    tally.changed += 1;
    let stored = if pause {
        store.set(INVITES_KEY, &true).await
    } else {
        store.delete(INVITES_KEY).await.map(drop)
    };
    if stored.is_err() {
        tally.failed += 1;
    }
    pause
}

async fn closed_by_server(ctx: &EventContext) -> Result<bool, DiscordError> {
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
            .with("invites", text(ctx, invites_key, &Args::new())),
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
        // A notice that does not arrive does not undo the change it tells of.
        let _ = ctx
            .api
            .execute(Action::SendMessage {
                channel_id: channel,
                content: Some(body.clone()),
                embeds: Vec::new(),
                reply_to: None,
                mentions: MentionPolicy::None,
            })
            .await;
    }
}

// ---- quarantine ----

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

/// Every quarantined member.
pub async fn held_all(store: &Store) -> Result<Vec<Held>, ModuleError> {
    let mut all = Vec::new();
    for index in 0..CHUNKS {
        all.extend(chunk(store, index).await?.0);
    }
    Ok(all)
}

async fn held_for(store: &Store, user: u64) -> Result<Vec<Held>, ModuleError> {
    Ok(held_all(store)
        .await?
        .into_iter()
        .filter(|held| held.user == user)
        .collect())
}

/// Records a quarantined member in the first chunk with room. Each write is checked against the
/// revision it was read at, so two joins do not lose each other.
async fn hold(store: &Store, held: Held) -> Result<(), ModuleError> {
    if held_all(store).await?.len() >= MAX_QUARANTINED {
        return Err(full());
    }
    for index in 0..CHUNKS {
        for _ in 0..CAS_TRIES {
            let (mut members, expect) = chunk(store, index).await?;
            if members.len() >= CHUNK_MEMBERS {
                break;
            }
            members.push(held);
            match store
                .replace(&chunk_key(index), &members, expect, None)
                .await
            {
                Ok(_) => return Ok(()),
                Err(StorageError::Conflict) => {}
                Err(error) => return Err(store_error(&error)),
            }
        }
    }
    Err(full())
}

fn full() -> ModuleError {
    ModuleError::other("the antiraid quarantine is full")
}

/// Forgets a member, wherever it is recorded.
async fn forget(store: &Store, user: u64) -> Result<(), ModuleError> {
    for index in 0..CHUNKS {
        for _ in 0..CAS_TRIES {
            let (members, expect) = chunk(store, index).await?;
            if !members.iter().any(|held| held.user == user) {
                break;
            }
            let rest: Vec<Held> = members
                .into_iter()
                .filter(|held| held.user != user)
                .collect();
            match store.replace(&chunk_key(index), &rest, expect, None).await {
                Ok(_) => break,
                Err(StorageError::Conflict) => {}
                Err(error) => return Err(store_error(&error)),
            }
        }
    }
    Ok(())
}

/// Gives the quarantine role (and the marker role) to a member who joins at a level that has one.
/// The member is recorded first; if a role cannot be given, the record is taken back.
async fn quarantine_join(
    ctx: &EventContext,
    memory: &Memory,
    cfg: &Antiraid,
    user: UserId,
) -> Result<(), ModuleError> {
    let store = ctx.store();
    let (current, _) = level(store, cfg.calm()).await?;
    let Some(quarantine) = cfg
        .effects
        .get(&current.name)
        .and_then(|effect| effect.quarantine)
    else {
        return Ok(());
    };
    if !held_for(store, user.get()).await?.is_empty() {
        return Ok(());
    }
    let role = ensure_role(ctx, RestrictionRole::Quarantine, quarantine.role)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    hold(
        store,
        Held {
            user: user.get(),
            role: role.get(),
            marker: quarantine.marker.map_or(0, RoleId::get),
        },
    )
    .await?;
    let mut given = give(ctx, memory, user, role).await;
    if let (Ok(()), Some(marker)) = (&given, quarantine.marker) {
        given = give(ctx, memory, user, marker).await;
    }
    if let Err(error) = given {
        forget(store, user.get()).await?;
        return Err(error);
    }
    Ok(())
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

/// Takes the quarantine role and the marker role off a member, and forgets it once both are off.
pub async fn release(
    ctx: &EventContext,
    memory: &Memory,
    user: UserId,
) -> Result<Tally, ModuleError> {
    let store = ctx.store();
    let mut tally = Tally::default();
    let held = held_for(store, user.get()).await?;
    if held.is_empty() {
        return Ok(tally);
    }
    let mut all_taken = true;
    for record in held {
        for role in [record.role, record.marker]
            .into_iter()
            .filter_map(RoleId::new_checked)
        {
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
    }
    if all_taken {
        forget(store, user.get()).await?;
        tally.changed += 1;
    }
    Ok(tally)
}

/// Releases every quarantined member.
pub async fn release_all(ctx: &EventContext, memory: &Memory) -> Result<Tally, ModuleError> {
    let users: BTreeSet<u64> = held_all(ctx.store())
        .await?
        .iter()
        .map(|held| held.user)
        .collect();
    let mut total = Tally::default();
    for user in users.into_iter().filter_map(UserId::new_checked) {
        let tally = release(ctx, memory, user).await?;
        total.changed += tally.changed;
        total.failed += tally.failed;
    }
    Ok(total)
}

/// Quarantines a member who joins, and forgets one who leaves. Bots are never quarantined.
pub async fn member_event(
    ctx: &EventContext,
    memory: &Memory,
    cfg: Option<&Antiraid>,
    event: &Event,
) -> Result<(), ModuleError> {
    let Some(cfg) = cfg else {
        return Ok(());
    };
    match event {
        Event::MemberJoined(info) if !info.user.bot => {
            quarantine_join(ctx, memory, cfg, info.user.id).await
        }
        Event::MemberLeft { user, .. } => forget(ctx.store(), user.id.get()).await,
        _ => Ok(()),
    }
}

/// How many members are quarantined now.
pub async fn quarantined_count(store: &Store) -> Result<usize, ModuleError> {
    Ok(held_all(store).await?.len())
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
