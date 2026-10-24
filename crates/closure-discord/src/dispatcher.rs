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
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use closure_core::action::InteractionReply;
use closure_core::capability::{Capability, CapabilitySet};
use closure_core::client::{ApiClient, DiscordApi};
use closure_core::error::ModuleError;
use closure_core::event::{CommandInvocation, Event, GuildId};
use closure_core::locale::{Args, Locale, Translator};
use closure_core::module::{EventContext, Module, ModuleId};
use closure_core::registry::ModuleRegistry;
use closure_core::settings::{ModuleSettings, SettingsProvider};
use closure_i18n::{CORE_NAMESPACE, user_message};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use futures_util::FutureExt;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::breaker::{Breaker, BreakerConfig};

/// How long the dispatcher waits for a reply to a user before giving up on it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct DispatcherConfig {
    pub breaker: BreakerConfig,
    /// A guild with nothing pending for this long releases its lanes.
    pub idle_after: Duration,
    /// How often idle guilds are looked for.
    pub reap_every: Duration,
}

impl Default for DispatcherConfig {
    fn default() -> Self {
        Self {
            breaker: BreakerConfig::default(),
            idle_after: Duration::from_secs(60),
            reap_every: Duration::from_secs(30),
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
}

impl GuildState {
    fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            queued_events: AtomicUsize::new(0),
            queued_commands: AtomicUsize::new(0),
            last_activity: Mutex::new(Instant::now()),
            breakers: Mutex::new(HashMap::new()),
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
        let index = if matches!(event, Event::Command(_)) {
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
    config: DispatcherConfig,
    guilds: DashMap<GuildId, Arc<GuildLanes>>,
    pending_total: AtomicUsize,
    closing: AtomicBool,
    cancel: CancellationToken,
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
        Event::Command(invocation) => invocation.user.id.get(),
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
        for module in registry.iter() {
            module
                .on_load()
                .await
                .map_err(|source| DispatchError::ModuleLoad {
                    module: module.manifest().id.clone(),
                    source,
                })?;
        }

        let shared = Arc::new(Shared {
            registry,
            settings,
            api,
            translator,
            config,
            guilds: DashMap::new(),
            pending_total: AtomicUsize::new(0),
            closing: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
        });
        shared.tracker.spawn(reaper(Arc::clone(&shared)));
        Ok(Self { shared })
    }

    /// Queues an event. Never blocks: when the guild already has `queue_capacity` events
    /// waiting or running, the event is dropped and counted.
    pub fn dispatch(&self, event: Event) {
        let kind = event.kind().as_str();
        if self.shared.closing.load(Ordering::Acquire) {
            metrics::counter!("closure_events_dropped_total", "reason" => "shutdown").increment(1);
            return;
        }
        metrics::counter!("closure_events_total", "kind" => kind).increment(1);

        let guild = event.guild_id();
        let limits = self.shared.settings.limits(guild);
        let Some(lanes) = self.lanes_for(guild, limits.concurrency) else {
            metrics::counter!("closure_events_dropped_total", "reason" => "shutdown").increment(1);
            return;
        };

        // The budget is the guild's, not a lane's: one busy channel may use all of it.
        let command = matches!(event, Event::Command(_));
        let budget = lanes.state.budget(command);
        if budget.fetch_add(1, Ordering::AcqRel) >= limits.queue_capacity {
            budget.fetch_sub(1, Ordering::AcqRel);
            lanes.state.pending.fetch_sub(1, Ordering::AcqRel);
            metrics::counter!("closure_events_dropped_total", "reason" => "queue_full")
                .increment(1);
            tracing::warn!(%guild, kind, "event dropped: the guild's queue is full");
            return;
        }

        let sender = lanes.sender_for(&event).clone();
        let job = Job {
            event: Arc::new(event),
            enqueued: Instant::now(),
            command,
        };
        self.shared.pending_total.fetch_add(1, Ordering::AcqRel);
        if sender.try_send(job).is_err() {
            lanes.state.finish(command);
            self.shared.pending_total.fetch_sub(1, Ordering::AcqRel);
            metrics::counter!("closure_events_dropped_total", "reason" => "closed").increment(1);
            tracing::warn!(%guild, kind, "event dropped: the lane is closed");
        }
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

        let lanes = match self.shared.guilds.entry(guild) {
            Entry::Occupied(mut entry) => {
                let replace = entry.get().event_lanes != concurrency
                    && entry.get().state.pending.load(Ordering::Acquire) == 0;
                // `shutdown` sets `closing` before it clears the map, and this check runs under
                // the same shard lock, so a lane can never be created behind its back.
                if replace && !self.shared.closing.load(Ordering::Acquire) {
                    entry.insert(self.spawn_lanes(guild, concurrency));
                }
                Arc::clone(entry.get())
            }
            Entry::Vacant(entry) => {
                if self.shared.closing.load(Ordering::Acquire) {
                    return None;
                }
                let lanes = self.spawn_lanes(guild, concurrency);
                entry.insert(Arc::clone(&lanes));
                lanes
            }
        };
        lanes.state.pending.fetch_add(1, Ordering::AcqRel);
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
                "dispatcher did not drain in time"
            );
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
    while let Some(job) = receiver.recv().await {
        metrics::histogram!("closure_event_queue_seconds")
            .record(job.enqueued.elapsed().as_secs_f64());
        let handled = AssertUnwindSafe(handle_event(&shared, guild, &state, &job.event))
            .catch_unwind()
            .await;
        if handled.is_err() {
            // handle_event isolates module panics itself; reaching this is a dispatcher bug,
            // and the lane must survive it.
            tracing::error!(%guild, kind = job.event.kind().as_str(), "dispatcher panicked while handling an event");
        }
        state.finish(job.command);
        shared.pending_total.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn handle_event(
    shared: &Arc<Shared>,
    guild: GuildId,
    state: &Arc<GuildState>,
    event: &Arc<Event>,
) {
    if let Event::Command(invocation) = &**event {
        handle_command(shared, guild, state, event, invocation).await;
        return;
    }
    let kind = event.kind();
    let calls = shared
        .registry
        .subscribers(kind)
        .map(|module| call_module(shared, guild, state, Arc::clone(module), event));
    futures_util::future::join_all(calls).await;
}

/// How a module call ended, as far as the breaker and the user are concerned.
#[derive(Debug)]
enum Verdict {
    Done,
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
    let config = shared.config.breaker;
    if !state.with_breaker(&id, |breaker| breaker.allow(Instant::now(), &config)) {
        metrics::counter!("closure_handler_total", "module" => id.to_string(), "result" => "skipped")
            .increment(1);
        return Verdict::Skipped;
    }

    let ctx = context_for(shared, guild, &id, &settings);
    // Read per call, so a changed timeout applies to the very next event.
    let handler_timeout = shared.settings.limits(guild).handler_timeout;
    let started = Instant::now();
    let mut task = tokio::spawn({
        let event = Arc::clone(event);
        async move { module.on_event(&ctx, &event).await }
    });
    let verdict = match tokio::time::timeout(handler_timeout, &mut task).await {
        Ok(Ok(Ok(()))) => Verdict::Done,
        Ok(Ok(Err(error))) => Verdict::Failed(error),
        Ok(Err(join_error)) => {
            tracing::error!(module = %id, %guild, kind = event.kind().as_str(), panic = join_error.is_panic(), "module task failed");
            Verdict::Panicked
        }
        Err(_elapsed) => {
            task.abort();
            tracing::error!(module = %id, %guild, kind = event.kind().as_str(), timeout = ?handler_timeout, "module timed out");
            Verdict::TimedOut
        }
    };

    record_verdict(state, &id, guild, event, &verdict, &config);
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
    .scoped_to(guild);
    EventContext::new(
        id.clone(),
        guild,
        api,
        shared.settings.locale(guild),
        shared.settings.policy(guild),
        Arc::clone(&settings.config),
        Arc::clone(&shared.translator),
    )
}

/// Updates the breaker and logs. An error from Discord (forbidden, not found, ...) means the
/// module did its job and Discord said no, so it does not count against the module.
fn record_verdict(
    state: &GuildState,
    id: &ModuleId,
    guild: GuildId,
    event: &Event,
    verdict: &Verdict,
    config: &BreakerConfig,
) {
    let now = Instant::now();
    let (result, blame_module) = match verdict {
        Verdict::Done => ("ok", false),
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
            breaker.record_failure(now, config);
            if !was_open && breaker.is_open(now) {
                tracing::error!(module = %id, %guild, "module is failing repeatedly; pausing it for this guild");
            }
        } else {
            breaker.record_success();
        }
    });
}

async fn handle_command(
    shared: &Arc<Shared>,
    guild: GuildId,
    state: &Arc<GuildState>,
    event: &Arc<Event>,
    invocation: &CommandInvocation,
) {
    let locale = invocation
        .locale
        .unwrap_or_else(|| shared.settings.locale(guild));
    let Some(module) = shared.registry.command_owner(&invocation.command) else {
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
        Verdict::Done => return,
        Verdict::Failed(ModuleError::Discord(error)) => {
            user_message(shared.translator.as_ref(), locale, &error)
        }
        Verdict::Failed(_) | Verdict::Panicked | Verdict::TimedOut | Verdict::Skipped => {
            text(shared, locale, "error-command-failed")
        }
    };
    reply(shared, invocation, message).await;
}

fn text(shared: &Shared, locale: Locale, key: &str) -> String {
    shared
        .translator
        .translate(locale, CORE_NAMESPACE, key, &Args::new())
}

/// Tells the invoker something went wrong. If the module already acknowledged the command, a
/// follow-up is used instead; failures here are only logged because there is nobody left to tell.
async fn reply(shared: &Shared, invocation: &CommandInvocation, message: String) {
    let granted: CapabilitySet = [Capability::RespondToInteractions].into_iter().collect();
    let client = ApiClient::new(Arc::clone(&shared.api), granted, ModuleId::system());
    let reply = InteractionReply {
        content: Some(message),
        embeds: Vec::new(),
        ephemeral: true,
    };
    let attempt = async {
        if client.respond(invocation, reply.clone()).await.is_err() {
            client.followup(invocation, reply).await
        } else {
            Ok(())
        }
    };
    match tokio::time::timeout(REPLY_TIMEOUT, attempt).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "could not tell the user a command failed"),
        Err(_) => tracing::warn!("timed out telling the user a command failed"),
    }
}

#[cfg(test)]
#[path = "dispatcher_tests.rs"]
mod tests;
