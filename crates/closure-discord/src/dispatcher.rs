//! Delivers events to modules.
//!
//! - Each guild has up to `concurrency` *lanes*. An event goes to the lane of the channel, user
//!   or role it concerns, so everything about one channel (a message and its later deletion) is
//!   handled in order while unrelated activity runs in parallel.
//! - Lanes have bounded queues; when one is full the event is dropped and counted rather than
//!   letting memory grow behind a slow module.
//! - Every module call runs in its own task with a timeout; a panic or a hang costs only that
//!   call, and a module that keeps failing in a guild is skipped for a while (circuit breaker).

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::action::InteractionReply;
use closure_core::capability::{Capability, CapabilitySet};
use closure_core::cases::{Case, CaseBackend, CaseError, Cases, MemoryCases, NewCase};
use closure_core::client::{ApiClient, DiscordApi};
use closure_core::error::ModuleError;
use closure_core::event::{
    AutocompleteInvocation, CommandInvocation, ContextTarget, Event, GuildId, InteractionRef,
    MessageInfo, UserId,
};
use closure_core::history::{HistoryBackend, MemoryHistory, MessageHistory};
use closure_core::interaction::{InteractionGuilds, InteractionLimits, Recorded};
use closure_core::latency::GatewayLatency;
use closure_core::locale::{Args, Locale, Translator};
use closure_core::module::{CommandSpec, ContextKind, EventContext, Module, ModuleId};
use closure_core::registry::ModuleRegistry;
use closure_core::schedule::{MemoryScheduler, ScheduledJob, Scheduler, SchedulerBackend};
use closure_core::settings::{ModuleSettings, SettingsProvider};
use closure_core::storage::{MemoryStorage, StorageBackend, Store};
use closure_core::subscription::Flow;
use closure_core::throttle::LogGate;
use closure_i18n::{CORE_NAMESPACE, user_message};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use futures_util::FutureExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

use crate::breaker::{Breaker, BreakerConfig, Ticket};
use crate::text_commands::{Reader, Reading, TextCommandApi};

/// How long `shutdown` waits for lanes to stop once they were told to abort.
const HARD_STOP_WAIT: Duration = Duration::from_secs(2);

/// How long an aborted module task gets to actually stop. Shorter than [`HARD_STOP_WAIT`], so a
/// module that blocks its thread is reported by the lane before `shutdown` gives up on it.
const TASK_STOP_WAIT: Duration = Duration::from_secs(1);

/// How long the dispatcher waits for a reply to a user before giving up on it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// How far apart the messages about a full queue are; a raid fills it for as long as it lasts,
/// and each dropped event would otherwise be a line.
const QUEUE_FULL_LOG_EVERY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct DispatcherConfig {
    pub breaker: BreakerConfig,
    /// A guild with nothing pending for this long releases its lanes.
    pub idle_after: Duration,
    /// How often idle guilds are looked for.
    pub reap_every: Duration,
    /// How many interactions may be waiting for an answer; see [`InteractionLimits`].
    pub interaction_limits: InteractionLimits,
    /// The bot's own user id (its application id), given to every module call. `None` when
    /// unknown, as in tests that do not need it.
    pub bot_id: Option<UserId>,
    /// The Gateway's heartbeat round trips, given to every module call. Empty unless the
    /// Gateway writes into the same value.
    pub gateway_latency: Arc<GatewayLatency>,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            breaker: BreakerConfig::default(),
            idle_after: Duration::from_secs(60),
            reap_every: Duration::from_secs(30),
            interaction_limits: InteractionLimits::default(),
            bot_id: None,
            gateway_latency: Arc::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("module `{module}` failed to load: {source}")]
    ModuleLoad {
        module: ModuleId,
        source: ModuleError,
    },
}

struct Job {
    event: Arc<Event>,
    enqueued: Instant,
    /// Commands have their own lanes and their own share of the queue budget.
    command: bool,
    /// Where to say how a scheduled job went; dropped unsent if the job was lost on the way.
    outcome: Option<oneshot::Sender<JobOutcome>>,
}

/// How a scheduled job went, for whoever keeps the jobs (see [`Dispatcher::run_job`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    /// The module handled it.
    Done,
    /// The module failed, panicked or timed out: try again later.
    Failed(String),
    /// Nobody could run it for a reason that is not the job's (the module is switched off or
    /// paused, the queue is full, the bot is stopping): try again later, without counting.
    Postponed,
    /// The module does not exist any more: the job can never run.
    Gone,
    /// The module said it cannot handle this job (`Flow::Stop`), for example because a newer
    /// version no longer has the handler: it is not tried again, and a repeating one stops.
    Dropped,
}

/// Lanes a long queue of ordinary events can never occupy, so that slash commands are answered
/// within Discord's three second window even in the middle of a raid.
fn command_lane_count(concurrency: usize) -> usize {
    (concurrency / 4).max(1)
}

/// Room in each lane's channel. The real limit is the guild-wide budget checked on dispatch;
/// this only has to be larger than any allowed `queue_capacity`. Channels allocate lazily.
const LANE_CHANNEL_CAPACITY: usize = 10_000;

/// State shared by the lanes of one guild.
struct GuildState {
    /// Events accepted and not finished (waiting or running), of either kind.
    pending: AtomicUsize,
    queued_events: AtomicUsize,
    queued_commands: AtomicUsize,
    last_activity: Mutex<Instant>,
    breakers: Mutex<HashMap<ModuleId, Breaker>>,
    queue_full_log: LogGate,
}

impl GuildState {
    fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            queued_events: AtomicUsize::new(0),
            queued_commands: AtomicUsize::new(0),
            last_activity: Mutex::new(Instant::now()),
            breakers: Mutex::new(HashMap::new()),
            queue_full_log: LogGate::new(QUEUE_FULL_LOG_EVERY),
        }
    }

    /// The budget `command` events are counted against.
    fn budget(&self, command: bool) -> &AtomicUsize {
        if command {
            &self.queued_commands
        } else {
            &self.queued_events
        }
    }

    fn finish(&self, command: bool) {
        self.budget(command).fetch_sub(1, Ordering::AcqRel);
        self.touch();
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }

    fn touch(&self) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .elapsed()
    }

    fn with_breaker<R>(&self, module: &ModuleId, f: impl FnOnce(&mut Breaker) -> R) -> R {
        let mut breakers = self.breakers.lock().unwrap_or_else(PoisonError::into_inner);
        f(breakers.entry(module.clone()).or_default())
    }
}

/// The send side of a guild's lanes. Dropping it lets the lane workers drain and exit.
struct GuildLanes {
    /// Lanes for ordinary events; the senders after them are the command lanes.
    event_lanes: usize,
    senders: Vec<mpsc::Sender<Job>>,
    state: Arc<GuildState>,
}

impl GuildLanes {
    fn sender_for(&self, event: &Event) -> &mpsc::Sender<Job> {
        let key = ordering_key(event);
        let index = if event.interaction_token().is_some() {
            self.event_lanes + lane_index(key, self.senders.len() - self.event_lanes)
        } else {
            lane_index(key, self.event_lanes)
        };
        &self.senders[index]
    }
}

struct Shared {
    registry: Arc<ModuleRegistry>,
    settings: Arc<dyn SettingsProvider>,
    api: Arc<dyn DiscordApi>,
    translator: Arc<dyn Translator>,
    storage: Arc<dyn StorageBackend>,
    scheduler: Arc<dyn SchedulerBackend>,
    cases: Arc<dyn CaseBackend>,
    history: Arc<dyn HistoryBackend>,
    config: DispatcherConfig,
    guilds: DashMap<GuildId, Arc<GuildLanes>>,
    /// The guild of every live interaction, so a module cannot answer another guild's.
    interactions: Arc<InteractionGuilds>,
    /// Per module, how many of its calls overran the timeout and are still running, because
    /// they block a thread and cannot be aborted. A module with any is not given new calls.
    wedged: DashMap<ModuleId, Arc<AtomicUsize>>,
    pending_total: AtomicUsize,
    closing: AtomicBool,
    cancel: CancellationToken,
    /// Cancelled when the grace period of `shutdown` ran out: lanes stop taking work and running
    /// module calls are aborted, so nothing is still running when the modules are unloaded.
    abort: CancellationToken,
    tracker: TaskTracker,
}

/// Cheap to clone; all clones share the same lanes.
#[derive(Clone)]
pub struct Dispatcher {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("guilds", &self.shared.guilds.len())
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}

tokio::task_local! {
    /// Whether the module call running in this task was given a `CaseCreated`. A case recorded
    /// then is not announced again, or two modules answering each other's cases would loop.
    static HANDLING_CASE_ANNOUNCEMENT: bool;
}

/// Records cases through `inner`, then queues a `CaseCreated` for the guild. Queuing never waits,
/// and the modules that get the event cannot fail the recording.
struct NotifyingCases {
    inner: Arc<dyn CaseBackend>,
    shared: Weak<Shared>,
}

#[async_trait]
impl CaseBackend for NotifyingCases {
    async fn create(
        &self,
        module: &ModuleId,
        guild: GuildId,
        case: NewCase,
    ) -> Result<Case, CaseError> {
        let created = self.inner.create(module, guild, case).await?;
        let chained = HANDLING_CASE_ANNOUNCEMENT
            .try_with(|handling| *handling)
            .unwrap_or(false);
        // Gone only once the dispatcher is dropped, when nobody is left to tell.
        if !chained && let Some(shared) = self.shared.upgrade() {
            Dispatcher { shared }.dispatch(Event::CaseCreated(created.clone()));
        }
        Ok(created)
    }

    async fn get(&self, guild: GuildId, number: i64) -> Result<Option<Case>, CaseError> {
        self.inner.get(guild, number).await
    }

    async fn list_for_target(
        &self,
        guild: GuildId,
        target: UserId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        self.inner
            .list_for_target(guild, target, before, limit)
            .await
    }

    async fn list_recent(
        &self,
        guild: GuildId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        self.inner.list_recent(guild, before, limit).await
    }

    async fn revoke(
        &self,
        guild: GuildId,
        number: i64,
        by: UserId,
        reason: Option<String>,
    ) -> Result<Case, CaseError> {
        self.inner.revoke(guild, number, by, reason).await
    }
}

/// How an event concerns a particular channel, user or role: events with the same key are
/// handled in order.
fn ordering_key(event: &Event) -> u64 {
    match event {
        Event::MemberJoined(member) | Event::MemberUpdated(member) => member.user.id.get(),
        Event::MemberLeft { user, .. }
        | Event::BanAdded { user, .. }
        | Event::BanRemoved { user, .. } => user.id.get(),
        Event::MessageCreated(message) | Event::MessageUpdated(message) => message.channel_id.get(),
        Event::MessageDeleted { channel_id, .. }
        | Event::MessagesBulkDeleted { channel_id, .. } => channel_id.get(),
        Event::ReactionAdded(reaction) | Event::ReactionRemoved(reaction) => {
            reaction.channel_id.get()
        }
        Event::ChannelCreated(channel)
        | Event::ChannelUpdated(channel)
        | Event::ChannelDeleted(channel) => channel.id.get(),
        Event::RoleCreated(role) | Event::RoleUpdated(role) => role.id.get(),
        Event::RoleDeleted { role_id, .. } => role_id.get(),
        Event::RolesChanged(change) => change.user.id.get(),
        Event::ThreadCreated(thread)
        | Event::ThreadArchived(thread)
        | Event::ThreadUnarchived(thread) => thread.id.get(),
        Event::ThreadDeleted { thread_id, .. } => thread_id.get(),
        Event::CaseCreated(case) => case.target.get(),
        Event::Command(invocation) => invocation.user.id.get(),
        Event::Autocomplete(query) => query.user.id.get(),
        Event::ContextCommand(invocation) => invocation.user.id.get(),
        Event::Component(click) => click.user.id.get(),
        Event::Modal(submission) => submission.user.id.get(),
        Event::Scheduled(job) => u64::try_from(job.id).unwrap_or(0),
        Event::GuildAvailable { .. } | Event::GuildRemoved { .. } => 0,
    }
}

fn lane_index(key: u64, lanes: usize) -> usize {
    // Snowflakes carry a counter in their low bits; mix before taking the remainder so lanes
    // fill evenly.
    let mixed = key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32;
    usize::try_from(mixed % lanes as u64).unwrap_or(0)
}

impl Dispatcher {
    /// Loads the modules in dependency order and starts accepting events. A module that fails
    /// to load stops startup.
    pub async fn start(
        registry: Arc<ModuleRegistry>,
        settings: Arc<dyn SettingsProvider>,
        api: Arc<dyn DiscordApi>,
        translator: Arc<dyn Translator>,
        config: DispatcherConfig,
    ) -> Result<Self, DispatchError> {
        Self::start_with_storage(
            registry,
            settings,
            api,
            translator,
            config,
            Arc::new(MemoryStorage::default()),
        )
        .await
    }

    /// [`Self::start`] with the place modules keep their data (see [`closure_core::storage`]).
    /// Without it, data lives in memory and is gone when the bot stops.
    pub async fn start_with_storage(
        registry: Arc<ModuleRegistry>,
        settings: Arc<dyn SettingsProvider>,
        api: Arc<dyn DiscordApi>,
        translator: Arc<dyn Translator>,
        config: DispatcherConfig,
        storage: Arc<dyn StorageBackend>,
    ) -> Result<Self, DispatchError> {
        Self::start_with_services(
            registry,
            settings,
            api,
            translator,
            config,
            storage,
            Arc::new(MemoryScheduler::default()),
            Arc::new(MemoryCases::default()),
            Arc::new(MemoryHistory::default()),
        )
        .await
    }

    /// [`Self::start_with_storage`] with the place scheduled jobs, moderation cases and the
    /// message history are kept as well. Whoever keeps the jobs runs them with [`Self::run_job`].
    // Each service is its own argument: one struct for them would change every caller.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_services(
        registry: Arc<ModuleRegistry>,
        settings: Arc<dyn SettingsProvider>,
        api: Arc<dyn DiscordApi>,
        translator: Arc<dyn Translator>,
        config: DispatcherConfig,
        storage: Arc<dyn StorageBackend>,
        scheduler: Arc<dyn SchedulerBackend>,
        cases: Arc<dyn CaseBackend>,
        history: Arc<dyn HistoryBackend>,
    ) -> Result<Self, DispatchError> {
        for module in registry.iter() {
            module
                .on_load()
                .await
                .map_err(|source| DispatchError::ModuleLoad {
                    module: module.manifest().id.clone(),
                    source,
                })?;
        }

        let interactions = Arc::new(InteractionGuilds::with_limits(config.interaction_limits));
        let api: Arc<dyn DiscordApi> = Arc::new(TextCommandApi::new(
            api,
            Arc::clone(&interactions),
            Arc::clone(&settings),
            Arc::clone(&translator),
        ));
        // The case backend needs the shared state it is part of, so it holds a weak reference.
        let shared = Arc::new_cyclic(|weak| Shared {
            registry,
            settings,
            api,
            translator,
            storage,
            scheduler,
            cases: Arc::new(NotifyingCases {
                inner: cases,
                shared: Weak::clone(weak),
            }),
            history,
            config,
            guilds: DashMap::new(),
            interactions,
            wedged: DashMap::new(),
            pending_total: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            abort: CancellationToken::new(),
            tracker: TaskTracker::new(),
        });
        shared.tracker.spawn(reaper(Arc::clone(&shared)));
        Ok(Self { shared })
    }

    /// Queues an event. Never blocks: when the guild already has `queue_capacity` events
    /// waiting or running, the event is dropped and counted.
    pub fn dispatch(&self, event: Event) {
        self.enqueue(event, None);
    }

    /// [`Self::dispatch`], and a message that is a text command also runs as that command (see
    /// [`crate::text_commands`]). `permissions` is asked for only when a command runs.
    pub fn dispatch_message(
        &self,
        event: Event,
        permissions: impl FnOnce(&MessageInfo) -> Permissions,
    ) {
        let reading = match &event {
            Event::MessageCreated(message) => self.read_text_command(message, permissions),
            _ => Reading::Ignore,
        };
        self.enqueue(event, None);
        match reading {
            Reading::Ignore => {}
            Reading::Run(invocation) => {
                self.enqueue(Event::Command(invocation), None);
            }
            Reading::Usage { invocation, text } => self.answer_usage(invocation, text),
        }
    }

    fn read_text_command(
        &self,
        message: &MessageInfo,
        permissions: impl FnOnce(&MessageInfo) -> Permissions,
    ) -> Reading {
        // The application a text command runs as is the bot; without its id, nothing runs.
        let Some(bot) = self.shared.config.bot_id else {
            return Reading::Ignore;
        };
        let guild = message.guild_id;
        let policy = self.shared.settings.policy(guild);
        let Some(prefix) = policy.prefix.as_deref() else {
            return Reading::Ignore;
        };
        if !message.content.starts_with(prefix) {
            return Reading::Ignore;
        }
        let commands: Vec<&CommandSpec> = self
            .shared
            .registry
            .commands()
            .filter(|(module, _)| {
                self.shared
                    .settings
                    .module(guild, module)
                    .is_some_and(|settings| settings.enabled)
            })
            .map(|(_, spec)| spec)
            .collect();
        Reader {
            prefix,
            application: Id::new(bot.get()),
            locale: self.shared.settings.locale(guild),
            commands: &commands,
            translator: self.shared.translator.as_ref(),
        }
        .read(message, permissions)
    }

    /// Says what a text command should have looked like. The answer is ephemeral, so it does not
    /// stay in the channel.
    fn answer_usage(&self, invocation: CommandInvocation, text: String) {
        if self.shared.closing.load(Ordering::Acquire) {
            return;
        }
        let shared = Arc::clone(&self.shared);
        self.shared.tracker.spawn(async move {
            reply(&shared, &invocation, text).await;
        });
    }

    /// Hands a due job to its module and says how it went. The job takes the same way an event
    /// does (the guild's lane and queue, the module's timeout and breaker).
    pub async fn run_job(&self, job: ScheduledJob) -> JobOutcome {
        let (sender, receiver) = oneshot::channel();
        if !self.enqueue(Event::Scheduled(job), Some(sender)) {
            return JobOutcome::Postponed;
        }
        receiver.await.unwrap_or(JobOutcome::Postponed)
    }

    /// Whether the event was accepted.
    fn enqueue(&self, event: Event, outcome: Option<oneshot::Sender<JobOutcome>>) -> bool {
        let kind = event.kind().as_str();
        if self.shared.closing.load(Ordering::Acquire) {
            metrics::counter!("closure_events_dropped_total", "reason" => "shutdown").increment(1);
            return false;
        }
        metrics::counter!("closure_events_total", "kind" => kind).increment(1);

        let guild = event.guild_id();
        let limits = self.shared.settings.limits(guild);
        let Some(lanes) = self.lanes_for(guild, limits.concurrency) else {
            metrics::counter!("closure_events_dropped_total", "reason" => "shutdown").increment(1);
            return false;
        };

        // The budget is the guild's, not a lane's: one busy channel may use all of it.
        // Commands, clicks and forms are answered to a person who is waiting, within three
        // seconds, so they have lanes and a budget of their own.
        let command = event.interaction_token().is_some();
        let budget = lanes.state.budget(command);
        if budget.fetch_add(1, Ordering::AcqRel) >= limits.queue_capacity {
            budget.fetch_sub(1, Ordering::AcqRel);
            lanes.state.pending.fetch_sub(1, Ordering::AcqRel);
            metrics::counter!("closure_events_dropped_total", "reason" => "queue_full")
                .increment(1);
            log_queue_full(&lanes.state, guild, kind);
            return false;
        }

        // Once the event is accepted and before any module can see it: the record decides whose
        // interaction a module may answer. Events dropped above never use up a record.
        if let Some(token) = event.interaction_token() {
            let recorded = self.shared.interactions.register(token, guild);
            if !recorded.is_recorded() {
                // No module could answer it, so none is given it. Undo what was reserved above.
                budget.fetch_sub(1, Ordering::AcqRel);
                lanes.state.pending.fetch_sub(1, Ordering::AcqRel);
                let reason = match recorded {
                    Recorded::GuildFull => "interaction_records_guild_full",
                    Recorded::TableFull | Recorded::New | Recorded::Known => {
                        "interaction_records_table_full"
                    }
                };
                metrics::counter!("closure_events_dropped_total", "reason" => reason).increment(1);
                return false;
            }
        }

        let sender = lanes.sender_for(&event).clone();
        let job = Job {
            event: Arc::new(event),
            enqueued: Instant::now(),
            command,
            outcome,
        };
        self.shared.pending_total.fetch_add(1, Ordering::AcqRel);
        if sender.try_send(job).is_err() {
            lanes.state.finish(command);
            self.shared.pending_total.fetch_sub(1, Ordering::AcqRel);
            metrics::counter!("closure_events_dropped_total", "reason" => "closed").increment(1);
            tracing::warn!(%guild, kind, "event dropped: the lane is closed");
            return false;
        }
        true
    }

    /// The lanes of `guild`, created on first use and rebuilt when the configured number of
    /// lanes changed while nothing was pending. The guild's pending count is raised under the
    /// map lock so the reaper cannot release lanes that are about to receive an event.
    /// Returns `None` once shutdown has begun, so no lanes are created that nothing would drain.
    fn lanes_for(&self, guild: GuildId, concurrency: usize) -> Option<Arc<GuildLanes>> {
        let concurrency = concurrency.max(1);
        if let Some(existing) = self.shared.guilds.get(&guild)
            && existing.event_lanes == concurrency
        {
            existing.state.pending.fetch_add(1, Ordering::AcqRel);
            return Some(Arc::clone(&existing));
        }

        // `pending` is raised inside each arm, while the map entry is still locked: a concurrent
        // caller that wants to replace the lanes then sees them as busy, and the reaper does too.
        let lanes = match self.shared.guilds.entry(guild) {
            Entry::Occupied(mut entry) => {
                let replace = entry.get().event_lanes != concurrency
                    && entry.get().state.pending.load(Ordering::Acquire) == 0;
                // `shutdown` sets `closing` before it clears the map, and this check runs under
                // the same shard lock, so a lane can never be created behind its back.
                if replace && !self.shared.closing.load(Ordering::Acquire) {
                    entry.insert(self.spawn_lanes(guild, concurrency));
                }
                let lanes = Arc::clone(entry.get());
                lanes.state.pending.fetch_add(1, Ordering::AcqRel);
                lanes
            }
            Entry::Vacant(entry) => {
                if self.shared.closing.load(Ordering::Acquire) {
                    return None;
                }
                let lanes = self.spawn_lanes(guild, concurrency);
                lanes.state.pending.fetch_add(1, Ordering::AcqRel);
                entry.insert(Arc::clone(&lanes));
                lanes
            }
        };
        Some(lanes)
    }

    fn spawn_lanes(&self, guild: GuildId, concurrency: usize) -> Arc<GuildLanes> {
        let total = concurrency + command_lane_count(concurrency);
        let state = Arc::new(GuildState::new());
        let mut senders = Vec::with_capacity(total);
        for _ in 0..total {
            let (sender, receiver) = mpsc::channel(LANE_CHANNEL_CAPACITY);
            senders.push(sender);
            self.shared.tracker.spawn(lane_worker(
                Arc::clone(&self.shared),
                guild,
                Arc::clone(&state),
                receiver,
            ));
        }
        Arc::new(GuildLanes {
            event_lanes: concurrency,
            senders,
            state,
        })
    }

    /// Whether `module` has calls that overran their timeout and cannot be stopped because they
    /// block a thread. Such a module is not given new calls until they have returned.
    #[must_use]
    pub fn is_wedged(&self, module: &ModuleId) -> bool {
        self.shared.is_wedged(module)
    }

    /// Events accepted but not finished yet.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.shared.pending_total.load(Ordering::Acquire)
    }

    /// Guilds that currently hold lanes.
    #[must_use]
    pub fn active_guilds(&self) -> usize {
        self.shared.guilds.len()
    }

    /// Waits until every accepted event has been handled.
    pub async fn flush(&self) {
        while self.pending() > 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Stops accepting events, lets queued ones finish for up to `grace`, then unloads the
    /// modules in reverse dependency order. Returns whether everything drained in time.
    pub async fn shutdown(&self, grace: Duration) -> bool {
        self.shared.closing.store(true, Ordering::Release);
        self.shared.cancel.cancel();
        // Dropping the senders lets each lane finish its queue and exit.
        self.shared.guilds.clear();
        self.shared.tracker.close();
        let drained = tokio::time::timeout(grace, self.shared.tracker.wait())
            .await
            .is_ok();
        if !drained {
            tracing::warn!(
                ?grace,
                pending = self.pending(),
                "dispatcher did not drain in time; stopping what is still running"
            );
            // Without this the lanes would keep handling events, and a running module call
            // would carry on, after the modules below have been unloaded.
            self.shared.abort.cancel();
            if tokio::time::timeout(HARD_STOP_WAIT, self.shared.tracker.wait())
                .await
                .is_err()
            {
                tracing::error!(
                    "lanes did not stop after being told to; a module is blocking a thread"
                );
            }
        }

        let modules: Vec<&Arc<dyn Module>> = self.shared.registry.iter().collect();
        for module in modules.into_iter().rev() {
            module.on_unload().await;
        }
        drained
    }
}

async fn reaper(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(shared.config.reap_every);
    interval.tick().await;
    loop {
        tokio::select! {
            () = shared.cancel.cancelled() => return,
            _ = interval.tick() => {}
        }
        let idle_after = shared.config.idle_after;
        shared.guilds.retain(|_, lanes| {
            lanes.state.pending.load(Ordering::Acquire) > 0 || lanes.state.idle_for() < idle_after
        });
    }
}

async fn lane_worker(
    shared: Arc<Shared>,
    guild: GuildId,
    state: Arc<GuildState>,
    mut receiver: mpsc::Receiver<Job>,
) {
    loop {
        let job = tokio::select! {
            biased;
            () = shared.abort.cancelled() => break,
            job = receiver.recv() => job,
        };
        let Some(mut job) = job else { break };
        metrics::histogram!("closure_event_queue_seconds")
            .record(job.enqueued.elapsed().as_secs_f64());
        let handled = AssertUnwindSafe(handle_event(&shared, guild, &state, &job.event))
            .catch_unwind()
            .await;
        match handled {
            Ok(outcome) => {
                if let (Some(sender), Some(outcome)) = (job.outcome.take(), outcome) {
                    // Nobody waiting any more is fine: the job is then handed out again.
                    let _ = sender.send(outcome);
                }
            }
            Err(_) => {
                // handle_event isolates module panics itself; reaching this is a dispatcher bug,
                // and the lane must survive it.
                tracing::error!(%guild, kind = job.event.kind().as_str(), "dispatcher panicked while handling an event");
            }
        }
        settle(&shared, &state, &job);
    }
    // Jobs still queued when the lane was told to stop would be dropped with the receiver and
    // never counted off, which would leave `pending` (and so `flush`) stuck for good.
    drain_closed(&mut receiver, |job| settle(&shared, &state, &job)).await;
}

/// Closes `receiver` and hands every message that was or will be sent to `each`.
///
/// `try_recv` is not enough: a sender that already took its capacity but has not pushed the
/// message yet looks like an empty queue, and its message would be dropped unseen with the
/// receiver. `recv` returns `None` only once all such senders are done.
async fn drain_closed<T>(receiver: &mut mpsc::Receiver<T>, mut each: impl FnMut(T)) {
    receiver.close();
    while let Some(message) = receiver.recv().await {
        each(message);
    }
}

/// Counts an accepted job off once it was handled or discarded.
fn settle(shared: &Shared, state: &GuildState, job: &Job) {
    state.finish(job.command);
    shared.pending_total.fetch_sub(1, Ordering::AcqRel);
}

/// Says that events are dropped because the guild's queue is full, but not for every one of
/// them (see [`QUEUE_FULL_LOG_EVERY`]). Whether it said so.
fn log_queue_full(state: &GuildState, guild: GuildId, kind: &str) -> bool {
    let Some(held_back) = state.queue_full_log.pass() else {
        return false;
    };
    tracing::warn!(%guild, kind, held_back, "events are dropped: the guild's queue is full");
    true
}

impl Shared {
    fn is_wedged(&self, module: &ModuleId) -> bool {
        self.wedged
            .get(module)
            .is_some_and(|count| count.load(Ordering::Acquire) > 0)
    }

    /// Takes note of an aborted call that did not stop: it blocks a thread and keeps running
    /// until it returns on its own. The module gets no new calls meanwhile, or each of them
    /// would take another worker thread until the runtime has none left for anyone.
    fn wedge(&self, module: &ModuleId, task: JoinHandle<Result<Flow, ModuleError>>) {
        let count = Arc::clone(&*self.wedged.entry(module.clone()).or_default());
        let now = count.fetch_add(1, Ordering::AcqRel) + 1;
        metrics::gauge!("closure_module_wedged_calls", "module" => module.to_string())
            .set(f64::from(u32::try_from(now).unwrap_or(u32::MAX)));
        if now == 1 {
            tracing::error!(%module, "a module blocks a thread; it gets no new calls until its running ones return");
        }
        let module = module.clone();
        tokio::spawn(async move {
            // Ends when the blocked call finally returns and the abort takes effect.
            let _ = task.await;
            let left = count.fetch_sub(1, Ordering::AcqRel) - 1;
            metrics::gauge!("closure_module_wedged_calls", "module" => module.to_string())
                .set(f64::from(u32::try_from(left).unwrap_or(u32::MAX)));
            if left == 0 {
                tracing::warn!(%module, "a module that blocked a thread has returned; it gets calls again");
            }
        });
    }
}

/// Aborts a module task and waits until it is really gone: `abort` only requests cancellation,
/// and the modules must not be unloaded while a handler (or its destructors) still runs.
/// Whether it stopped; one that did not blocks a thread.
async fn stop_task<T>(task: &mut JoinHandle<T>, module: &ModuleId) -> bool {
    task.abort();
    let stopped = tokio::time::timeout(TASK_STOP_WAIT, task).await.is_ok();
    if !stopped {
        tracing::error!(%module, "a module task did not stop after being aborted; it is blocking a thread");
    }
    stopped
}

async fn handle_event(
    shared: &Arc<Shared>,
    guild: GuildId,
    state: &Arc<GuildState>,
    event: &Arc<Event>,
) -> Option<JobOutcome> {
    match &**event {
        Event::Command(invocation) => {
            let owner = shared.registry.command_owner(&invocation.command);
            handle_interaction(shared, guild, state, event, owner, invocation).await;
            return None;
        }
        Event::ContextCommand(invocation) => {
            let kind = match invocation.target {
                ContextTarget::User(_) => ContextKind::User,
                ContextTarget::Message(_) => ContextKind::Message,
            };
            let owner = shared.registry.context_owner(&invocation.command, kind);
            handle_interaction(shared, guild, state, event, owner, invocation).await;
            return None;
        }
        Event::Autocomplete(query) => {
            let owner = shared.registry.command_owner(&query.command);
            handle_autocomplete(shared, guild, state, event, owner, query).await;
            return None;
        }
        Event::Component(click) => {
            let owner = shared.registry.interaction_owner(&click.custom_id);
            handle_interaction(shared, guild, state, event, owner, click).await;
            return None;
        }
        Event::Modal(submission) => {
            let owner = shared.registry.interaction_owner(&submission.custom_id);
            handle_interaction(shared, guild, state, event, owner, submission).await;
            return None;
        }
        Event::Scheduled(job) => {
            return Some(handle_job(shared, guild, state, event, job).await);
        }
        _ => {}
    }
    // Modules with the same priority run together; the groups follow each other, highest first,
    // and a module that stops the event keeps it from the groups after its own.
    let receivers: Vec<(Arc<dyn Module>, i16)> = shared
        .registry
        .subscriptions(event.kind())
        .into_iter()
        .filter(|(_, subscription)| subscription.filter.matches(event))
        .map(|(module, subscription)| (Arc::clone(module), subscription.priority))
        .collect();
    for group in receivers.chunk_by(|a, b| a.1 == b.1) {
        let calls = group
            .iter()
            .map(|(module, _)| call_module(shared, guild, state, Arc::clone(module), event));
        let verdicts = futures_util::future::join_all(calls).await;
        if verdicts
            .iter()
            .any(|verdict| matches!(verdict, Verdict::Done(Flow::Stop)))
        {
            break;
        }
    }
    None
}

/// A due job goes to the module that scheduled it, whatever the subscriptions say.
async fn handle_job(
    shared: &Arc<Shared>,
    guild: GuildId,
    state: &Arc<GuildState>,
    event: &Arc<Event>,
    job: &ScheduledJob,
) -> JobOutcome {
    let Some(module) = shared.registry.get(&job.module) else {
        return JobOutcome::Gone;
    };
    match call_module(shared, guild, state, Arc::clone(module), event).await {
        Verdict::Done(Flow::Stop) => JobOutcome::Dropped,
        Verdict::Done(_) => JobOutcome::Done,
        Verdict::Failed(error) => JobOutcome::Failed(error.to_string()),
        Verdict::Panicked => JobOutcome::Failed("the handler panicked".to_owned()),
        Verdict::TimedOut => JobOutcome::Failed("the handler timed out".to_owned()),
        Verdict::Skipped => JobOutcome::Postponed,
    }
}

/// How a module call ended, as far as the breaker and the user are concerned.
#[derive(Debug)]
enum Verdict {
    Done(Flow),
    /// The module ran but returned an error.
    Failed(ModuleError),
    Panicked,
    TimedOut,
    /// The breaker is open, the module is disabled or unknown.
    Skipped,
}

async fn call_module(
    shared: &Arc<Shared>,
    guild: GuildId,
    state: &Arc<GuildState>,
    module: Arc<dyn Module>,
    event: &Arc<Event>,
) -> Verdict {
    let id = module.manifest().id.clone();
    let Some(settings) = shared.settings.module(guild, &id).filter(|s| s.enabled) else {
        return Verdict::Skipped;
    };
    if shared.is_wedged(&id) {
        metrics::counter!("closure_handler_total", "module" => id.to_string(), "result" => "wedged")
            .increment(1);
        return Verdict::Skipped;
    }
    let config = shared.config.breaker;
    let Some(ticket) = state.with_breaker(&id, |breaker| breaker.allow(Instant::now())) else {
        metrics::counter!("closure_handler_total", "module" => id.to_string(), "result" => "skipped")
            .increment(1);
        return Verdict::Skipped;
    };

    let ctx = context_for(shared, guild, &id, &settings);
    // Read per call, so a changed timeout applies to the very next event.
    let handler_timeout = shared.settings.limits(guild).handler_timeout;
    let started = Instant::now();
    let announcing = matches!(**event, Event::CaseCreated(_));
    let mut task = tokio::spawn({
        let event = Arc::clone(event);
        async move {
            HANDLING_CASE_ANNOUNCEMENT
                .scope(announcing, module.on_event_flow(&ctx, &event))
                .await
        }
    });
    let waited = tokio::select! {
        result = tokio::time::timeout(handler_timeout, &mut task) => Some(result),
        () = shared.abort.cancelled() => None,
    };
    let Some(waited) = waited else {
        if !stop_task(&mut task, &id).await {
            shared.wedge(&id, task);
        }
        return Verdict::Skipped;
    };
    let verdict = match waited {
        Ok(Ok(Ok(flow))) => Verdict::Done(flow),
        Ok(Ok(Err(error))) => Verdict::Failed(error),
        Ok(Err(join_error)) => {
            tracing::error!(module = %id, %guild, kind = event.kind().as_str(), panic = join_error.is_panic(), "module task failed");
            Verdict::Panicked
        }
        Err(_elapsed) => {
            if !stop_task(&mut task, &id).await {
                shared.wedge(&id, task);
            }
            tracing::error!(module = %id, %guild, kind = event.kind().as_str(), timeout = ?handler_timeout, "module timed out");
            Verdict::TimedOut
        }
    };

    record_verdict(state, &id, guild, event, ticket, &verdict, &config);
    metrics::histogram!("closure_handler_seconds", "module" => id.to_string())
        .record(started.elapsed().as_secs_f64());
    verdict
}

fn context_for(
    shared: &Shared,
    guild: GuildId,
    id: &ModuleId,
    settings: &ModuleSettings,
) -> EventContext {
    let api = ApiClient::new(
        Arc::clone(&shared.api),
        settings.granted.clone(),
        id.clone(),
    )
    .with_interactions(Arc::clone(&shared.interactions))
    .scoped_to(guild);
    let context = EventContext::new(
        id.clone(),
        guild,
        api,
        shared.settings.locale(guild),
        shared.settings.policy(guild),
        Arc::clone(&settings.config),
        Arc::clone(&shared.translator),
    )
    .with_permissions(shared.settings.permissions(guild))
    .with_scheduler(Scheduler::new(
        Arc::clone(&shared.scheduler),
        &settings.granted,
        id.clone(),
        guild,
    ))
    .with_store(Store::new(
        Arc::clone(&shared.storage),
        &settings.granted,
        id.clone(),
        guild,
    ))
    .with_cases(Cases::new(
        Arc::clone(&shared.cases),
        &settings.granted,
        id.clone(),
        guild,
    ))
    .with_history(MessageHistory::new(
        Arc::clone(&shared.history),
        &settings.granted,
        id.clone(),
        guild,
    ))
    .with_gateway_latency(Arc::clone(&shared.config.gateway_latency));
    match shared.config.bot_id {
        Some(bot) => context.with_bot(bot),
        None => context,
    }
}

/// Updates the breaker and logs. An error from Discord (forbidden, not found, ...) means the
/// module did its job and Discord said no, so it does not count against the module.
fn record_verdict(
    state: &GuildState,
    id: &ModuleId,
    guild: GuildId,
    event: &Event,
    ticket: Ticket,
    verdict: &Verdict,
    config: &BreakerConfig,
) {
    let now = Instant::now();
    let (result, blame_module) = match verdict {
        Verdict::Done(_) => ("ok", false),
        Verdict::Failed(ModuleError::Discord(_)) => ("discord_error", false),
        Verdict::Failed(_) => ("error", true),
        Verdict::Panicked => ("panic", true),
        Verdict::TimedOut => ("timeout", true),
        Verdict::Skipped => return,
    };
    metrics::counter!("closure_handler_total", "module" => id.to_string(), "result" => result)
        .increment(1);

    if let Verdict::Failed(error) = verdict {
        tracing::warn!(module = %id, %guild, kind = event.kind().as_str(), %error, "module returned an error");
    }
    state.with_breaker(id, |breaker| {
        if blame_module {
            let was_open = breaker.is_open(now);
            breaker.record_failure(ticket, now, config);
            if !was_open && breaker.is_open(now) {
                tracing::error!(module = %id, %guild, "module is failing repeatedly; pausing it for this guild");
            }
        } else {
            breaker.record_success(ticket);
        }
    });
}

/// A command, a click or a submitted form goes to the one module that owns it, and the person
/// who sent it is told in their language if that module cannot answer.
async fn handle_interaction(
    shared: &Arc<Shared>,
    guild: GuildId,
    state: &Arc<GuildState>,
    event: &Arc<Event>,
    owner: Option<&Arc<dyn Module>>,
    invocation: &dyn InteractionRef,
) {
    let locale = invocation
        .locale()
        .unwrap_or_else(|| shared.settings.locale(guild));
    let Some(module) = owner else {
        reply(
            shared,
            invocation,
            text(shared, locale, "error-command-failed"),
        )
        .await;
        return;
    };
    let id = module.manifest().id.clone();
    if shared
        .settings
        .module(guild, &id)
        .is_none_or(|settings| !settings.enabled)
    {
        reply(
            shared,
            invocation,
            text(shared, locale, "error-module-disabled"),
        )
        .await;
        return;
    }

    let verdict = call_module(shared, guild, state, Arc::clone(module), event).await;
    let message = match verdict {
        Verdict::Done(_) => return,
        Verdict::Failed(ModuleError::Discord(error)) => {
            user_message(shared.translator.as_ref(), locale, &error)
        }
        Verdict::Failed(_) | Verdict::Panicked | Verdict::TimedOut | Verdict::Skipped => {
            text(shared, locale, "error-command-failed")
        }
    };
    reply(shared, invocation, message).await;
}

/// Discord shows the user nothing when no choices come within three seconds, so the module
/// gets less than that and the answer is given before Discord gives up.
const AUTOCOMPLETE_DEADLINE: Duration = Duration::from_millis(2_500);

/// An autocomplete request goes to the module that owns the command, in the same lane and under
/// the same breaker as a command. Whatever goes wrong, the user gets no choices: Discord has no
/// place for an error there.
async fn handle_autocomplete(
    shared: &Arc<Shared>,
    guild: GuildId,
    state: &Arc<GuildState>,
    event: &Arc<Event>,
    owner: Option<&Arc<dyn Module>>,
    query: &AutocompleteInvocation,
) {
    let Some(module) = owner else {
        return answer_without_choices(shared, query).await;
    };
    let id = module.manifest().id.clone();
    if shared
        .settings
        .module(guild, &id)
        .is_none_or(|settings| !settings.enabled)
    {
        return answer_without_choices(shared, query).await;
    }
    let call = call_module(shared, guild, state, Arc::clone(module), event);
    // A module that answered is done; one that did not gets the empty answer.
    if !matches!(
        tokio::time::timeout(AUTOCOMPLETE_DEADLINE, call).await,
        Ok(Verdict::Done(_))
    ) {
        answer_without_choices(shared, query).await;
    }
}

async fn answer_without_choices(shared: &Shared, query: &AutocompleteInvocation) {
    let granted: CapabilitySet = [Capability::RespondToInteractions].into_iter().collect();
    let client = ApiClient::new(Arc::clone(&shared.api), granted, ModuleId::system());
    match tokio::time::timeout(REPLY_TIMEOUT, client.autocomplete(query, Vec::new())).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "could not answer an autocomplete request"),
        Err(_) => tracing::warn!("timed out answering an autocomplete request"),
    }
}

fn text(shared: &Shared, locale: Locale, key: &str) -> String {
    shared
        .translator
        .translate(locale, CORE_NAMESPACE, key, &Args::new())
}

/// Tells the invoker something went wrong. If the module already acknowledged the command, a
/// follow-up is used instead; failures here are only logged because there is nobody left to tell.
async fn reply(shared: &Shared, invocation: &dyn InteractionRef, message: String) {
    let granted: CapabilitySet = [Capability::RespondToInteractions].into_iter().collect();
    let client = ApiClient::new(Arc::clone(&shared.api), granted, ModuleId::system());
    let reply = InteractionReply {
        content: Some(message),
        embeds: Vec::new(),
        components: Vec::new(),
        ephemeral: true,
    };
    let attempt = async {
        if client.respond(invocation, reply.clone()).await.is_err() {
            client.followup(invocation, reply).await
        } else {
            Ok(())
        }
    };
    // A text command's reply is refused in a channel the bot may not write to: nothing to act on.
    let text_command = invocation.token().text_command_message().is_some();
    match tokio::time::timeout(REPLY_TIMEOUT, attempt).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if text_command => {
            tracing::debug!(%error, "could not reply to a text command");
        }
        Ok(Err(error)) => tracing::warn!(%error, "could not tell the user a command failed"),
        Err(_) => tracing::warn!("timed out telling the user a command failed"),
    }
}

#[cfg(test)]
#[path = "dispatcher_tests.rs"]
mod tests;
