//! Running the gateway shards.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::event::{Event as ModuleEvent, GuildId};
use closure_core::latency::GatewayLatency;
use closure_core::locale::Locale;
use closure_core::registry::ModuleRegistry;
use futures_util::{FutureExt, StreamExt as FuturesStreamExt};
use serde_json::Value;
use tokio::sync::Notify;
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::error::{ReceiveMessageError, ReceiveMessageErrorType};
use twilight_gateway::{
    CloseFrame, Config, Event, EventTypeFlags, Intents, Message, Shard, ShardId, parse,
};
use twilight_http::Client;
use twilight_model::application::interaction::InteractionType;
use twilight_model::gateway::payload::incoming::GuildCreate;
use twilight_model::gateway::payload::outgoing::RequestGuildMembers;
use twilight_model::id::Id;
use twilight_model::id::marker::GuildMarker;

use crate::convert;
use crate::dispatcher::Dispatcher;
use crate::text_commands;

/// Wait between two member requests of a shard; a shard may send 120 commands a minute, and a whole-guild request takes a while to answer.
const MEMBER_REQUEST_INTERVAL: Duration = Duration::from_millis(500);

/// How long a hook may take before the gateway moves on without it.
const HOOK_TIMEOUT: Duration = Duration::from_secs(5);
/// Time the history may take to keep one message, fetching its attachments when a deletion or edit needs them; shorter than the lanes' wait (`RECORD_WAIT`, 25 seconds) so a lane usually hears the record is done before giving up.
const RECORD_TIMEOUT: Duration = Duration::from_secs(20);

/// Message events that may wait for their hook; past it the newest is dropped and logged, since keeping them all would let a slow database grow memory without bound.
const MESSAGE_QUEUE_CAPACITY: usize = 10_000;

/// The most a spill file holds. Past it the newest member event is dropped and logged.
const SPILL_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// Called by the gateway for guild arrivals and departures.
/// Hooks run in order on a background task, right after the event is handed to the dispatcher, so a slow database never stalls reading from the gateway (which would get the connection cut). A module may see an event a moment before its hook finished.
#[async_trait]
pub trait GuildHooks: Send + Sync {
    /// The bot can see the guild: on start-up for every guild, and when it is added to one.
    async fn guild_available(
        &self,
        _guild: GuildId,
        _name: &str,
        _preferred_locale: Option<Locale>,
    ) {
    }

    /// The bot was removed from the guild (not merely disconnected by an outage).
    async fn guild_removed(&self, _guild: GuildId) {}

    /// Whether the guild's messages are wanted now (see [`Self::message`]); asked before every message event is copied, so it must be a cheap read.
    fn wants_messages(&self, _guild: GuildId) -> bool {
        false
    }

    /// A message event of a guild that [`Self::wants_messages`] said yes to. Hooks of message events run in the order the events came, before the guild hooks waiting at the same time. A hook may change the event (the history sets what an edit took away); the modules get it as changed.
    async fn message(&self, _event: &mut ModuleEvent) {}

    async fn member_activity(&self, _event: &ModuleEvent) {}
}

/// Hooks that do nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

#[async_trait]
impl GuildHooks for NoHooks {}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("could not start the gateway: {0}")]
    Start(String),
}

#[derive(Clone)]
pub struct GatewayConfig {
    pub token: String,
    pub intents: Intents,
    /// Where the heartbeat round trips go; the same value the dispatcher hands the modules.
    pub latency: Arc<GatewayLatency>,
    /// Guilds with more members than this are not asked for all at once: they would be held in memory and sent in growing numbers. Their members are known as they speak and as the gateway reports them, and fetched when a check needs one. `0` asks for every guild's members.
    pub member_request_limit: u64,
    /// Where member events that do not fit a full queue are kept until the hooks catch up; without it they are dropped.
    pub member_spool: Option<PathBuf>,
    /// The only guilds the bot stays in; without it every guild is served.
    pub allowed_guilds: Option<AllowedGuilds>,
}

/// Takes the bot out of one guild.
pub trait GuildLeaver: Send + Sync {
    fn leave(&self, guild: GuildId);
}

/// Waits between the attempts to leave a guild: a failed leave is tried again after each of these.
const LEAVE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_mins(5),
];

/// Runs `attempt` again after each of `delays` while it fails; the last error is returned.
async fn retry_on_error<F, Fut, E>(delays: &[Duration], mut attempt: F) -> Result<(), E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
    E: fmt::Display,
{
    let mut waits = delays.iter();
    loop {
        match attempt().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let Some(delay) = waits.next() else {
                    return Err(error);
                };
                tracing::warn!(%error, ?delay, "could not leave a guild that is not allowed; trying again");
                tokio::time::sleep(*delay).await;
            }
        }
    }
}

/// Leaves a guild through Discord's HTTP API.
struct HttpLeaver(Arc<Client>);

impl GuildLeaver for HttpLeaver {
    fn leave(&self, guild: GuildId) {
        let http = Arc::clone(&self.0);
        tokio::spawn(async move {
            let left = retry_on_error(&LEAVE_RETRY_DELAYS, || {
                let http = Arc::clone(&http);
                async move { http.leave_guild(guild).await.map(|_| ()) }
            })
            .await;
            match left {
                Ok(()) => {
                    tracing::warn!(%guild, "left a guild that is not on the list of allowed guilds");
                }
                Err(error) => {
                    metrics::counter!("closure_guild_leave_failures_total").increment(1);
                    tracing::error!(%guild, %error, "could not leave a guild that is not allowed, even after retries");
                }
            }
        });
    }
}

/// The guilds the bot may serve, and how it gets out of any other.
#[derive(Clone)]
pub struct AllowedGuilds {
    guilds: Arc<HashSet<GuildId>>,
    leaver: Arc<dyn GuildLeaver>,
}

impl fmt::Debug for AllowedGuilds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AllowedGuilds")
            .field("guilds", &self.guilds.len())
            .finish_non_exhaustive()
    }
}

impl AllowedGuilds {
    #[must_use]
    pub fn new(guilds: HashSet<GuildId>, leaver: Arc<dyn GuildLeaver>) -> Self {
        Self {
            guilds: Arc::new(guilds),
            leaver,
        }
    }

    /// Leaves through `http`.
    #[must_use]
    pub fn leaving_with(guilds: HashSet<GuildId>, http: Arc<Client>) -> Self {
        Self::new(guilds, Arc::new(HttpLeaver(http)))
    }

    #[must_use]
    pub fn permits(&self, guild: GuildId) -> bool {
        self.guilds.contains(&guild)
    }
}

impl fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("intents", &self.intents)
            .finish_non_exhaustive()
    }
}

/// Intents to request: those the registered modules declare, plus `GUILDS` (without which the bot learns nothing of its servers). Intents belong to the connection, so this is the union over all registered modules, not the enabled ones.
#[must_use]
pub fn gateway_intents(registry: &ModuleRegistry) -> Intents {
    registry.all_intents() | Intents::GUILDS
}

/// A cache holding what the hierarchy checks need and nothing more.
#[must_use]
pub fn new_cache() -> Arc<InMemoryCache> {
    Arc::new(
        InMemoryCache::builder()
            .resource_types(
                ResourceType::GUILD
                    | ResourceType::MEMBER
                    | ResourceType::ROLE
                    | ResourceType::CHANNEL
                    | ResourceType::USER_CURRENT
                    | ResourceType::VOICE_STATE
                    | ResourceType::EMOJI
                    | ResourceType::STICKER
                    | ResourceType::STAGE_INSTANCE,
            )
            .build(),
    )
}

/// Which shards currently hold an active session; for readiness probes.
#[derive(Debug)]
pub struct GatewayHealth {
    shards: Vec<AtomicBool>,
}

impl GatewayHealth {
    fn new(shards: usize) -> Self {
        Self {
            shards: (0..shards).map(|_| AtomicBool::new(false)).collect(),
        }
    }

    fn set(&self, shard: ShardId, ready: bool) {
        let index = usize::try_from(shard.number()).unwrap_or(usize::MAX);
        if let Some(flag) = self.shards.get(index) {
            flag.store(ready, Ordering::Release);
        }
    }

    #[must_use]
    pub fn total(&self) -> usize {
        self.shards.len()
    }

    #[must_use]
    pub fn ready(&self) -> usize {
        self.shards
            .iter()
            .filter(|flag| flag.load(Ordering::Acquire))
            .count()
    }

    #[must_use]
    pub fn all_ready(&self) -> bool {
        self.total() > 0 && self.ready() == self.total()
    }

    /// Whether the shard holding `guild` has a session; a guild's pages need its own shard, which Discord assigns as `(id >> 22) % shards`.
    #[must_use]
    pub fn guild_ready(&self, guild: u64) -> bool {
        let total = self.shards.len() as u64;
        if total == 0 {
            return false;
        }
        usize::try_from((guild >> 22) % total)
            .ok()
            .and_then(|index| self.shards.get(index))
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
}

/// The running gateway.
#[derive(Debug)]
pub struct GatewayHandle {
    tracker: TaskTracker,
    health: Arc<GatewayHealth>,
}

impl GatewayHandle {
    #[must_use]
    pub fn health(&self) -> &Arc<GatewayHealth> {
        &self.health
    }

    /// Completes when every shard has stopped: after shutdown was requested, or because Discord closed them for good (invalid token, disallowed intents).
    pub async fn closed(&self) {
        self.tracker.wait().await;
    }
}

struct ShardContext {
    cache: Arc<InMemoryCache>,
    dispatcher: Dispatcher,
    hooks: Arc<HookQueues>,
    /// Hooks for guild arrivals, departures and messages (see [`GuildHooks`]).
    guild_hooks: Arc<dyn GuildHooks>,
    health: Arc<GatewayHealth>,
    latency: Arc<GatewayLatency>,
    /// Whether the bot asked for members, which a guild's member list needs to be requested.
    members_intent: bool,
    member_request_limit: u64,
    allowed: Option<AllowedGuilds>,
    shutdown: CancellationToken,
}

impl Drop for ShardContext {
    /// Every shard task holds the context, so it is dropped when the last of them has finished, by shutdown or because Discord closed them for good.
    fn drop(&mut self) {
        self.hooks.close();
    }
}

/// A guild arrival or departure waiting for its hook to run.
#[derive(Debug)]
enum HookJob {
    Available {
        guild: GuildId,
        name: String,
        preferred_locale: Option<Locale>,
    },
    Removed {
        guild: GuildId,
    },
}

impl HookJob {
    fn guild(&self) -> GuildId {
        match self {
            Self::Available { guild, .. } | Self::Removed { guild } => *guild,
        }
    }
}

/// The newest pending hook of each guild. Hooks bring persistent state in line with the guilds the bot is in, so only the latest word about a guild matters; this keeps the queue bounded by the number of guilds, and pushing never waits, so a slow database cannot cut the connection.
#[derive(Default)]
struct HookQueue {
    state: Mutex<HookQueueState>,
    wake: Notify,
    /// Where member events go when the queue is full (see [`Spill`]).
    spill: Option<Spill>,
}

#[derive(Default)]
struct HookQueueState {
    pending: HashMap<GuildId, HookJob>,
    /// Message events in the order they came; every one is kept up to the capacity.
    messages: VecDeque<(
        ModuleEvent,
        Option<tokio::sync::oneshot::Sender<ModuleEvent>>,
    )>,
    /// Set once no shard can push any more, or the worker stops; no message is accepted after that.
    closed: bool,
    /// Member events go to the spill file, not the queue, until the file has been worked off, so they stay in the order they came.
    spilling: bool,
}

/// What the worker takes in one go, so the queue cannot close between reading pending work and reading `closed`.
struct Taken {
    jobs: Vec<HookJob>,
    messages: Vec<(
        ModuleEvent,
        Option<tokio::sync::oneshot::Sender<ModuleEvent>>,
    )>,
    closed: bool,
}

impl HookQueue {
    fn push(&self, job: HookJob) {
        self.state().pending.insert(job.guild(), job);
        self.wake.notify_one();
    }

    /// Queues a message event for [`GuildHooks::message`]. Past capacity the event is dropped and logged by its kind alone, never its content.
    fn push_message(
        &self,
        event: ModuleEvent,
    ) -> Option<tokio::sync::oneshot::Receiver<ModuleEvent>> {
        let mut state = self.state();
        // Once closed, no worker is left to record the event, so no lane may wait for it.
        if state.closed {
            return None;
        }
        if state.messages.len() >= MESSAGE_QUEUE_CAPACITY {
            tracing::warn!(
                kind = event.kind().as_str(),
                "message hooks are behind; a message event was dropped"
            );
            return None;
        }
        let (done, recorded) = tokio::sync::oneshot::channel();
        state.messages.push_back((event, Some(done)));
        self.wake.notify_one();
        Some(recorded)
    }

    /// Queues a join or a leave for [`GuildHooks::member_activity`]. When the queue is full the event is written to the spill file instead; the worker plays it back once it has caught up, so a slow database delays these records without losing them.
    fn push_member_activity(&self, event: ModuleEvent) {
        let mut state = self.state();
        if state.closed {
            tracing::warn!("member activity hook queue is closed");
            return;
        }
        if state.spilling || state.messages.len() >= MESSAGE_QUEUE_CAPACITY {
            if self
                .spill
                .as_ref()
                .is_some_and(|spill| spill.append(&event))
            {
                state.spilling = true;
                self.wake.notify_one();
            } else {
                tracing::warn!("member activity hook queue is full; an event was dropped");
            }
            return;
        }
        state.messages.push_back((event, None));
        self.wake.notify_one();
    }

    /// A queue whose overflow of member events is kept in `path`; events left by an earlier run are played back first.
    fn with_spill(path: PathBuf) -> Self {
        let spill = Spill::new(path);
        let queue = Self {
            state: Mutex::new(HookQueueState {
                spilling: spill.has_events(),
                ..HookQueueState::default()
            }),
            wake: Notify::new(),
            spill: Some(spill),
        };
        queue.wake.notify_one();
        queue
    }

    /// Plays back what the spill file holds. Returns whether there may be more to do.
    async fn replay_spill(&self, hooks: &Arc<dyn GuildHooks>) -> bool {
        let Some(spill) = &self.spill else {
            return false;
        };
        let events = spill.take();
        if events.is_empty() {
            // What was taken held nothing readable: it is not kept, or the worker would come back to it for ever.
            spill.finish();
            // Nothing is left: events go to the queue again, checked under the queue's lock so none is written to the file in between.
            let mut state = self.state();
            if spill.has_events() {
                return true;
            }
            state.spilling = false;
            return false;
        }
        let mut again = Vec::new();
        for event in events {
            if run_member_activity(hooks.as_ref(), &event).await {
                again.push(event);
            }
        }
        spill.finish();
        // The ones that timed out wait for the next turn, after the ones written meanwhile.
        for event in &again {
            spill.append(event);
        }
        true
    }

    /// Declares that nothing will be pushed any more, so the worker may end once it is idle.
    fn close(&self) {
        self.state().closed = true;
        self.wake.notify_one();
    }

    /// Closes the queue and drops the senders still waiting, which lets their lanes go on; used once the worker has stopped, when nothing queued will ever be recorded.
    fn abandon(&self) {
        let mut state = self.state();
        state.closed = true;
        state.messages.clear();
    }

    /// Everything pending, and whether the queue is closed; all read under one lock, so a closed queue reporting no work is really finished.
    fn take(&self) -> Taken {
        let mut state = self.state();
        Taken {
            jobs: std::mem::take(&mut state.pending).into_values().collect(),
            messages: std::mem::take(&mut state.messages).into(),
            closed: state.closed,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HookQueueState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How many workers record messages. A guild always has the same one, so its events stay in order, and a guild whose database writes crawl holds up only the guilds sharing its lane.
const MESSAGE_LANES: usize = 8;

/// The queues the gateway feeds: one for guild arrivals and departures, so those never wait behind recorded messages, and [`MESSAGE_LANES`] for message and member-activity hooks.
struct HookQueues {
    lifecycle: Arc<HookQueue>,
    lanes: Vec<Arc<HookQueue>>,
}

impl HookQueues {
    /// With a directory, each lane keeps the member events that do not fit in it there.
    fn new(spool: Option<&Path>) -> Self {
        Self {
            lifecycle: Arc::default(),
            lanes: (0..MESSAGE_LANES)
                .map(|lane| {
                    Arc::new(spool.map_or_else(HookQueue::default, |dir| {
                        HookQueue::with_spill(dir.join(format!("member-activity-{lane}.jsonl")))
                    }))
                })
                .collect(),
        }
    }

    fn lane(&self, guild: GuildId) -> &Arc<HookQueue> {
        let lane = usize::try_from(guild.get() % MESSAGE_LANES as u64).unwrap_or(0);
        &self.lanes[lane]
    }

    fn close(&self) {
        self.lifecycle.close();
        for lane in &self.lanes {
            lane.close();
        }
    }

    /// Every queue with the worker that runs it.
    fn spawn_workers(&self, hooks: &Arc<dyn GuildHooks>, tracker: &TaskTracker) {
        for queue in std::iter::once(&self.lifecycle).chain(&self.lanes) {
            tracker.spawn(run_hook_worker(Arc::clone(hooks), Arc::clone(queue)));
        }
    }
}

/// Member events that did not fit a full queue, one JSON object to a line. Taking them moves the file aside first and removes it once played back, so a stop in the middle replays them on the next start.
struct Spill {
    path: PathBuf,
    replay: PathBuf,
    /// Writes and takes of the file go one at a time.
    turn: Mutex<()>,
}

impl Spill {
    fn new(path: PathBuf) -> Self {
        if let Some(dir) = path.parent()
            && let Err(error) = std::fs::create_dir_all(dir)
        {
            tracing::warn!(path = %dir.display(), %error, "could not create the spool directory; member events may not be kept");
        }
        let replay = path.with_extension("replay");
        Self {
            path,
            replay,
            turn: Mutex::new(()),
        }
    }

    fn turn(&self) -> std::sync::MutexGuard<'_, ()> {
        self.turn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn has_events(&self) -> bool {
        let _turn = self.turn();
        [&self.path, &self.replay]
            .iter()
            .any(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0))
    }

    /// Writes `event` at the end of the file; whether it was kept.
    fn append(&self, event: &ModuleEvent) -> bool {
        let _turn = self.turn();
        let Ok(mut line) = serde_json::to_vec(event) else {
            return false;
        };
        line.push(b'\n');
        let size = std::fs::metadata(&self.path).map_or(0, |meta| meta.len());
        if size + line.len() as u64 > SPILL_LIMIT_BYTES {
            return false;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| file.write_all(&line))
            .is_ok()
    }

    /// The events to play back: the ones an earlier stop left half played, else the ones written.
    fn take(&self) -> Vec<ModuleEvent> {
        let _turn = self.turn();
        if !self.replay.exists() && std::fs::rename(&self.path, &self.replay).is_err() {
            return Vec::new();
        }
        let text = std::fs::read_to_string(&self.replay).unwrap_or_default();
        text.lines()
            .filter_map(|line| {
                let event = serde_json::from_str(line).ok();
                if event.is_none() {
                    tracing::warn!(
                        "a member event in the spool file could not be read; it is dropped"
                    );
                }
                event
            })
            .collect()
    }

    /// The events taken were played back.
    fn finish(&self) {
        let _turn = self.turn();
        let _ = std::fs::remove_file(&self.replay);
    }
}

/// Runs the member-activity hook for one event; a hook that panics or takes too long is logged. The event is tried again on a timeout, which a slow database may cause, but not after a panic.
async fn run_member_activity(hooks: &dyn GuildHooks, event: &ModuleEvent) -> bool {
    let hook = AssertUnwindSafe(hooks.member_activity(event)).catch_unwind();
    match tokio::time::timeout(HOOK_TIMEOUT, hook).await {
        Ok(Ok(())) => false,
        Ok(Err(_)) => {
            tracing::error!("member activity hook panicked");
            false
        }
        Err(_) => {
            tracing::warn!("member activity hook timed out");
            true
        }
    }
}

/// Abandons the queue when the worker ends, whether it returns, panics or is aborted; the lanes wait for the messages they hold, so none may stay queued for a worker that is gone.
struct WorkerExit(Arc<HookQueue>);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.abandon();
    }
}

/// Runs queued hooks one at a time. Ends once the queue is closed and empty, so hooks of events handled during shutdown are not lost.
async fn run_hook_worker(hooks: Arc<dyn GuildHooks>, queue: Arc<HookQueue>) {
    let _exit = WorkerExit(Arc::clone(&queue));
    loop {
        let taken = queue.take();
        if taken.jobs.is_empty() && taken.messages.is_empty() {
            if queue.replay_spill(&hooks).await {
                continue;
            }
            if taken.closed {
                return;
            }
            queue.wake.notified().await;
            continue;
        }
        for (mut event, recorded) in taken.messages {
            if recorded.is_none() {
                let _ = run_member_activity(hooks.as_ref(), &event).await;
                continue;
            }
            // A panicking hook counts as not kept; the worker goes on with the next message.
            let hook = AssertUnwindSafe(hooks.message(&mut event)).catch_unwind();
            match tokio::time::timeout(RECORD_TIMEOUT, hook).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::error!(
                    kind = event.kind().as_str(),
                    "message hook panicked; the message was not kept"
                ),
                Err(_) => tracing::warn!(kind = event.kind().as_str(), "message hook timed out"),
            }
            // Sent whether or not the hook kept the message: the lane goes on either way.
            if let Some(recorded) = recorded {
                let _ = recorded.send(event);
            }
        }
        for job in taken.jobs {
            let hook = async {
                match &job {
                    HookJob::Available {
                        guild,
                        name,
                        preferred_locale,
                    } => hooks.guild_available(*guild, name, *preferred_locale).await,
                    HookJob::Removed { guild } => hooks.guild_removed(*guild).await,
                }
            };
            // A panicking hook is logged without its message; the worker goes on with the next job.
            match tokio::time::timeout(HOOK_TIMEOUT, AssertUnwindSafe(hook).catch_unwind()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::error!("guild hook panicked; the worker goes on"),
                Err(_) => tracing::warn!(?job, "guild hook timed out"),
            }
        }
    }
}

/// The guild a message event concerns; `None` for every other event.
fn message_guild(event: &ModuleEvent) -> Option<GuildId> {
    match event {
        ModuleEvent::MessageCreated(info) | ModuleEvent::MessageUpdated(info) => {
            Some(info.guild_id)
        }
        ModuleEvent::MessageDeleted { guild_id, .. }
        | ModuleEvent::MessagesBulkDeleted { guild_id, .. } => Some(*guild_id),
        _ => None,
    }
}

/// Queues a message event when its guild wants messages; the check comes first, so an event of a guild that keeps none is never copied.
fn offer_message(
    queue: &HookQueue,
    hooks: &dyn GuildHooks,
    event: &ModuleEvent,
) -> Option<tokio::sync::oneshot::Receiver<ModuleEvent>> {
    if message_guild(event).is_some_and(|guild| hooks.wants_messages(guild)) {
        return queue.push_message(event.clone());
    }
    None
}

/// A label for an error that is safe to log.
fn error_class(kind: &ReceiveMessageErrorType) -> &'static str {
    match kind {
        ReceiveMessageErrorType::Deserializing { .. } => "deserializing",
        ReceiveMessageErrorType::Reconnect => "reconnect",
        _ => "other",
    }
}

/// Whether an event means the shard has (or no longer has) a working session.
fn readiness_change(event: &Event) -> Option<bool> {
    match event {
        // `Resumed` is how a shard comes back after a reconnect that kept its session.
        Event::Ready(_) | Event::Resumed => Some(true),
        Event::GatewayClose(_) => Some(false),
        _ => None,
    }
}

/// Connects the recommended number of shards and feeds the cache and dispatcher until `shutdown` is cancelled.
pub async fn start_gateway(
    http: &Client,
    config: GatewayConfig,
    cache: Arc<InMemoryCache>,
    dispatcher: Dispatcher,
    hooks: Arc<dyn GuildHooks>,
    shutdown: CancellationToken,
) -> Result<GatewayHandle, GatewayError> {
    let gateway = Config::new(config.token, config.intents | Intents::GUILDS);
    // The shard count Discord recommends; all the shards run in this process.
    let recommended = http
        .gateway()
        .authed()
        .await
        .map_err(|error| GatewayError::Start(error.to_string()))?
        .model()
        .await
        .map_err(|error| GatewayError::Start(error.to_string()))?
        .shards;
    let shards: Vec<Shard> = twilight_gateway::bucket(0, 1, recommended)
        .map(|shard| Shard::with_config(shard, gateway.clone()))
        .collect();
    tracing::info!(shards = shards.len(), "starting gateway");

    let health = Arc::new(GatewayHealth::new(shards.len()));
    let context = Arc::new(ShardContext {
        cache,
        dispatcher,
        hooks: Arc::new(HookQueues::new(config.member_spool.as_deref())),
        guild_hooks: Arc::clone(&hooks),
        health: Arc::clone(&health),
        latency: config.latency,
        members_intent: config.intents.contains(Intents::GUILD_MEMBERS),
        member_request_limit: config.member_request_limit,
        allowed: config.allowed_guilds,
        shutdown,
    });
    let tracker = TaskTracker::new();
    context.hooks.spawn_workers(&hooks, &tracker);
    for shard in shards {
        tracker.spawn(run_shard(shard, Arc::clone(&context)));
    }
    tracker.close();
    Ok(GatewayHandle { tracker, health })
}

const MAX_MODAL_RAW_PAYLOAD_BYTES: usize = 64 * 1024;

async fn next_event_with_modal_raw(
    shard: &mut Shard,
) -> Option<Result<(Event, Option<Value>), ReceiveMessageError>> {
    loop {
        match shard.next().await? {
            Err(error) => return Some(Err(error)),
            Ok(Message::Close(frame)) => {
                return Some(Ok((Event::GatewayClose(frame), None)));
            }
            Ok(Message::Text(raw)) => {
                let interaction_text = raw.contains("INTERACTION_CREATE");
                let raw_size = raw.len();
                let oversized = interaction_text && raw_size > MAX_MODAL_RAW_PAYLOAD_BYTES;
                let retained_raw = (interaction_text && !oversized).then(|| raw.clone());
                match parse(raw, EventTypeFlags::all()) {
                    Ok(Some(event)) => {
                        let event = Event::from(event);
                        let components = match &event {
                            Event::InteractionCreate(interaction)
                                if interaction.0.kind == InteractionType::ModalSubmit =>
                            {
                                if oversized {
                                    tracing::warn!(
                                        size = raw_size,
                                        "discarding oversized modal submission payload"
                                    );
                                    Some(Value::Null)
                                } else if let Some(raw) = retained_raw.as_deref() {
                                    raw_modal_components(&event, raw)
                                } else {
                                    tracing::warn!("modal submission had no bounded raw payload");
                                    Some(Value::Null)
                                }
                            }
                            _ => None,
                        };
                        return Some(Ok((event, components)));
                    }
                    Ok(None) => {}
                    Err(error) => return Some(Err(error)),
                }
            }
        }
    }
}

fn raw_modal_components(event: &Event, raw: &str) -> Option<Value> {
    let Event::InteractionCreate(interaction) = event else {
        return None;
    };
    if interaction.0.kind != InteractionType::ModalSubmit {
        return None;
    }
    if raw.len() > MAX_MODAL_RAW_PAYLOAD_BYTES {
        tracing::warn!(
            size = raw.len(),
            "discarding oversized modal submission payload"
        );
        return Some(Value::Null);
    }
    let Ok(envelope) = serde_json::from_str::<Value>(raw) else {
        tracing::warn!("discarding malformed modal submission payload");
        return Some(Value::Null);
    };
    let components = envelope
        .get("d")
        .and_then(|dispatch| dispatch.get("data"))
        .and_then(|data| data.get("components"))
        .cloned()
        .unwrap_or(Value::Null);
    Some(components)
}

async fn run_shard(mut shard: Shard, context: Arc<ShardContext>) {
    let id = shard.id();
    let mut closing = false;
    let mut members = MemberRequests::default();
    let mut pacing = interval(MEMBER_REQUEST_INTERVAL);
    pacing.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        let item = tokio::select! {
            () = context.shutdown.cancelled(), if !closing => {
                closing = true;
                shard.close(CloseFrame::NORMAL);
                continue;
            }
            guild = members.next(&mut pacing), if !members.is_empty() => {
                if let Some(guild) = guild {
                    shard.command(&RequestGuildMembers::builder(guild).query("", None));
                }
                continue;
            }
            item = next_event_with_modal_raw(&mut shard) => item,
        };
        // The stream ends once the connection is closed for good.
        let Some(item) = item else { break };
        match item {
            Ok((event, raw_modal_components)) => {
                // Once shutdown was requested, the close handshake finishing is the end; the stream itself would otherwise reconnect, as it does after any close.
                let finished = closing && matches!(event, Event::GatewayClose(_));
                // `Shard` is not `Sync`, so only plain values cross the await points below.
                let latency = shard.latency().recent().first().copied();
                handle_event_with_modal_components(
                    &context,
                    id,
                    latency,
                    &event,
                    raw_modal_components.as_ref(),
                );
                if context.members_intent
                    && let Some(guild) = guild_to_request(&event, context.member_request_limit)
                {
                    // Discord leaves offline members out of a guild's first payload; asking caches them.
                    members.push(guild);
                }
                if finished {
                    break;
                }
            }
            // Never print the error itself: for a payload that fails to parse it contains the whole event, including interaction tokens and member data.
            Err(error) => tracing::warn!(
                shard = %id,
                class = error_class(error.kind()),
                "error receiving gateway event"
            ),
        }
    }
    context.health.set(id, false);
    if closing {
        tracing::info!(shard = %id, "shard closed");
    } else {
        tracing::error!(
            shard = %id,
            "shard was closed for good, e.g. by an invalid token or intents that are not enabled"
        );
    }
}

fn before(cache: &InMemoryCache, event: &Event) -> convert::Before {
    match event {
        Event::MemberUpdate(updated) => {
            let member = cache.member(updated.guild_id, updated.user.id);
            convert::Before {
                roles: member.as_ref().map(|member| member.roles().to_vec()),
                member: member.map(|member| {
                    (
                        member.nick().map(str::to_owned),
                        member.communication_disabled_until().map(convert::seconds),
                    )
                }),
                ..convert::Before::default()
            }
        }
        Event::ThreadUpdate(updated) => convert::Before {
            archived: cache.channel(updated.0.id).and_then(|channel| {
                channel
                    .thread_metadata
                    .as_ref()
                    .map(|metadata| metadata.archived)
            }),
            ..convert::Before::default()
        },
        Event::ChannelUpdate(updated) => convert::Before {
            channel: cache
                .channel(updated.0.id)
                .and_then(|cached| convert::channel(&cached)),
            ..convert::Before::default()
        },
        Event::RoleUpdate(updated) => convert::Before {
            role: cache
                .role(updated.role.id)
                .map(|cached| convert::role(updated.guild_id, cached.value().resource())),
            ..convert::Before::default()
        },
        Event::VoiceStateUpdate(update) => convert::Before {
            voice_channel: update
                .0
                .guild_id
                .and_then(|guild| cache.voice_state(update.0.user_id, guild))
                .map(|cached| cached.channel_id()),
            ..convert::Before::default()
        },
        Event::GuildEmojisUpdate(update) => convert::Before {
            emojis: cache.guild_emojis(update.guild_id).map(|ids| {
                ids.iter()
                    .filter_map(|id| cache.emoji(*id))
                    .map(|cached| {
                        convert::cached_emoji_info(update.guild_id, cached.value().resource())
                    })
                    .collect()
            }),
            ..convert::Before::default()
        },
        Event::GuildStickersUpdate(update) => convert::Before {
            stickers: cache.guild_stickers(update.guild_id).map(|ids| {
                ids.iter()
                    .filter_map(|id| cache.sticker(*id))
                    .map(|cached| {
                        convert::cached_sticker_info(update.guild_id, cached.value().resource())
                    })
                    .collect()
            }),
            ..convert::Before::default()
        },
        Event::StageInstanceUpdate(updated) => convert::Before {
            stage: cache
                .stage_instance(updated.0.id)
                .map(|cached| convert::stage_info(cached.value().resource())),
            ..convert::Before::default()
        },
        _ => convert::Before::default(),
    }
}

/// The guilds whose members are still to be asked for, one at a time.
#[derive(Default)]
struct MemberRequests {
    pending: VecDeque<Id<GuildMarker>>,
}

impl MemberRequests {
    /// Queues `guild` unless it already waits.
    fn push(&mut self, guild: Id<GuildMarker>) {
        if !self.pending.contains(&guild) {
            self.pending.push_back(guild);
        }
    }

    fn take_next(&mut self) -> Option<Id<GuildMarker>> {
        self.pending.pop_front()
    }

    async fn next(&mut self, pacing: &mut tokio::time::Interval) -> Option<Id<GuildMarker>> {
        pacing.tick().await;
        self.take_next()
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

/// The guild whose members to ask Discord for: one that just became available, unless it has more members than `limit` (`0` has none).
fn guild_to_request(event: &Event, limit: u64) -> Option<Id<GuildMarker>> {
    let Event::GuildCreate(created) = event else {
        return None;
    };
    match &**created {
        GuildCreate::Available(guild)
            if limit == 0 || guild.member_count.is_none_or(|count| count <= limit) =>
        {
            Some(guild.id)
        }
        GuildCreate::Available(_) | GuildCreate::Unavailable(_) => None,
    }
}

#[cfg(test)]
fn handle_event(context: &ShardContext, id: ShardId, latency: Option<Duration>, event: &Event) {
    handle_event_with_modal_components(context, id, latency, event, None);
}

fn handle_event_with_modal_components(
    context: &ShardContext,
    id: ShardId,
    latency: Option<Duration>,
    event: &Event,
    raw_modal_components: Option<&Value>,
) {
    if let (Some(allowed), Event::GuildCreate(created)) = (&context.allowed, event) {
        let guild = match &**created {
            GuildCreate::Available(guild) => guild.id,
            GuildCreate::Unavailable(guild) => guild.id,
        };
        if !allowed.permits(guild) {
            allowed.leaver.leave(guild);
            return;
        }
    }
    let before = before(&context.cache, event);
    context.cache.update(event);
    if let Some(ready) = readiness_change(event) {
        context.health.set(id, ready);
    }
    if matches!(event, Event::GatewayHeartbeatAck)
        && let Some(latency) = latency
    {
        metrics::gauge!("closure_gateway_latency_seconds", "shard" => id.number().to_string())
            .set(latency.as_secs_f64());
        context.latency.record(u64::from(id.number()), latency);
    }

    for converted in
        crate::convert::events_with_modal_components(event, &before, raw_modal_components)
    {
        if matches!(&converted, ModuleEvent::DirectMessageComponent(_)) {
            context.dispatcher.dispatch(converted);
            continue;
        }
        if let Some(allowed) = &context.allowed
            && !matches!(converted, ModuleEvent::GuildRemoved { .. })
            && !allowed.permits(converted.guild_id())
        {
            continue;
        }
        if matches!(&converted, ModuleEvent::MemberJoined(member) if !member.user.bot)
            || matches!(&converted, ModuleEvent::MemberLeft { user, .. } if !user.bot)
        {
            context
                .hooks
                .lane(converted.guild_id())
                .push_member_activity(converted.clone());
        }
        let recorded = offer_message(
            context.hooks.lane(converted.guild_id()),
            &*context.guild_hooks,
            &converted,
        );
        let hook = match &converted {
            ModuleEvent::GuildAvailable {
                guild_id,
                name,
                preferred_locale,
            } => Some(HookJob::Available {
                guild: *guild_id,
                name: name.clone(),
                preferred_locale: *preferred_locale,
            }),
            ModuleEvent::GuildRemoved { guild_id } => Some(HookJob::Removed { guild: *guild_id }),
            _ => None,
        };
        match recorded {
            Some(recorded) => {
                context
                    .dispatcher
                    .dispatch_message_after(converted, recorded, |message| {
                        text_commands::invoker_permissions(&context.cache, message)
                    });
            }
            None => context.dispatcher.dispatch_message(converted, |message| {
                text_commands::invoker_permissions(&context.cache, message)
            }),
        }
        if let Some(job) = hook {
            context.hooks.lifecycle.push(job);
        }
    }
}

#[cfg(test)]
mod tests {
    use closure_core::module::{Module, ModuleId, ModuleManifest};
    use closure_core::testing::StubModule;
    use serde_json::json;
    use twilight_model::channel::Channel;
    use twilight_model::gateway::payload::incoming::{
        MemberAdd, MemberUpdate, ThreadCreate, ThreadUpdate,
    };

    use super::*;

    #[test]
    fn health_counts_ready_shards() {
        let health = GatewayHealth::new(3);
        assert!(!health.all_ready());
        health.set(ShardId::new(0, 3), true);
        health.set(ShardId::new(2, 3), true);
        assert_eq!((health.ready(), health.total()), (2, 3));
        health.set(ShardId::new(1, 3), true);
        assert!(health.all_ready());
        health.set(ShardId::new(1, 3), false);
        assert!(!health.all_ready());
    }

    #[test]
    fn an_empty_gateway_is_never_ready() {
        assert!(!GatewayHealth::new(0).all_ready());
    }

    #[test]
    fn intents_are_the_modules_union_plus_guilds() {
        let manifest = ModuleManifest::new(ModuleId::new("m").unwrap(), "m", "0.1.0")
            .intents(Intents::GUILD_MODERATION);
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        let registry = ModuleRegistry::new(vec![module]).unwrap();
        assert_eq!(
            gateway_intents(&registry),
            Intents::GUILDS | Intents::GUILD_MODERATION
        );
        assert_eq!(
            gateway_intents(&ModuleRegistry::new(vec![]).unwrap()),
            Intents::GUILDS
        );
    }

    #[test]
    fn error_labels_never_contain_the_event() {
        let kind = ReceiveMessageErrorType::Deserializing {
            event: r#"{"token":"secret-interaction-token"}"#.into(),
        };
        let label = error_class(&kind);
        assert_eq!(label, "deserializing");
        assert!(!label.contains("secret"));
    }

    #[test]
    fn a_resumed_session_counts_as_ready_again() {
        assert_eq!(readiness_change(&Event::Resumed), Some(true));
        assert_eq!(readiness_change(&Event::GatewayClose(None)), Some(false));
        assert_eq!(readiness_change(&Event::GatewayHeartbeatAck), None);
    }

    fn user_json() -> serde_json::Value {
        json!({ "id": "7", "username": "alice", "discriminator": "0", "avatar": null, "bot": false })
    }

    #[test]
    fn the_state_before_an_update_is_read_before_the_cache_takes_it() {
        let cache = new_cache();
        let added: MemberAdd = serde_json::from_value(json!({
            "guild_id": "1", "user": user_json(), "roles": ["5"], "pending": false,
            "joined_at": "2026-10-24T00:00:00.000000+00:00", "deaf": false, "mute": false, "flags": 0
        }))
        .unwrap();
        cache.update(&Event::MemberAdd(Box::new(added)));
        let update: MemberUpdate = serde_json::from_value(json!({
            "guild_id": "1", "user": user_json(), "roles": ["6"], "pending": false
        }))
        .unwrap();
        let event = Event::MemberUpdate(Box::new(update.clone()));
        assert_eq!(
            before(&cache, &event).roles,
            Some(vec![twilight_model::id::Id::new(5)])
        );
        cache.update(&event);
        assert_eq!(
            before(&cache, &Event::MemberUpdate(Box::new(update))).roles,
            Some(vec![twilight_model::id::Id::new(6)]),
            "the cache has moved on, so this is the state it holds now"
        );
        let stranger: MemberUpdate = serde_json::from_value(json!({
            "guild_id": "1", "user": { "id": "8", "username": "bob", "discriminator": "0", "avatar": null, "bot": false },
            "roles": [], "pending": false
        }))
        .unwrap();
        assert_eq!(
            before(&cache, &Event::MemberUpdate(Box::new(stranger))).roles,
            None
        );
    }

    #[test]
    fn a_guild_that_is_unavailable_is_not_asked_for_its_members() {
        let unavailable: GuildCreate =
            serde_json::from_value(json!({ "id": "1", "unavailable": true })).unwrap();
        assert_eq!(
            guild_to_request(&Event::GuildCreate(Box::new(unavailable)), 0),
            None
        );
    }

    fn guild_with(members: u64) -> Event {
        let created: GuildCreate = serde_json::from_value(json!({
            "id": "1", "name": "big", "owner_id": "2", "afk_timeout": 300,
            "default_message_notifications": 0, "explicit_content_filter": 0, "features": [],
            "mfa_level": 0, "nsfw_level": 0, "preferred_locale": "en-US", "premium_tier": 0,
            "system_channel_flags": 0, "verification_level": 0, "premium_progress_bar_enabled": false, "roles": [], "emojis": [],
            "stickers": [], "channels": [], "threads": [], "members": [], "presences": [],
            "voice_states": [], "stage_instances": [], "guild_scheduled_events": [],
            "member_count": members, "large": true, "unavailable": false
        }))
        .unwrap();
        Event::GuildCreate(Box::new(created))
    }

    #[test]
    fn a_guild_past_the_limit_is_not_asked_for_all_its_members() {
        assert_eq!(
            guild_to_request(&guild_with(500_000), 500_000),
            Some(Id::new(1))
        );
        assert_eq!(guild_to_request(&guild_with(500_001), 500_000), None);
        assert_eq!(
            guild_to_request(&guild_with(9_000_000), 0),
            Some(Id::new(1))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn member_requests_are_spaced_by_the_interval() {
        let mut requests = MemberRequests::default();
        requests.push(Id::new(1));
        requests.push(Id::new(2));
        let mut pacing = interval(MEMBER_REQUEST_INTERVAL);
        pacing.set_missed_tick_behavior(MissedTickBehavior::Skip);

        assert_eq!(requests.next(&mut pacing).await, Some(Id::new(1)));
        assert!(
            tokio::time::timeout(Duration::from_millis(499), requests.next(&mut pacing))
                .await
                .is_err()
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(requests.next(&mut pacing).await, Some(Id::new(2)));
    }

    #[test]
    fn a_guild_waits_once_for_its_members_and_the_guilds_go_in_order() {
        let mut members = MemberRequests::default();
        members.push(Id::new(10));
        members.push(Id::new(11));
        members.push(Id::new(10));
        assert_eq!(members.take_next(), Some(Id::new(10)));
        assert_eq!(members.take_next(), Some(Id::new(11)));
        assert_eq!(members.take_next(), None);
        assert!(members.is_empty());
    }

    #[test]
    fn an_event_that_is_not_a_guild_is_not_asked_for_its_members() {
        assert_eq!(guild_to_request(&Event::GatewayHeartbeatAck, 0), None);
    }

    #[test]
    fn the_archived_state_of_a_cached_thread_is_read_before_it_changes() {
        let cache = new_cache();
        let channel: Channel = serde_json::from_value(json!({
            "id": "20", "type": 11, "guild_id": "1", "name": "t", "parent_id": "2",
            "thread_metadata": {
                "archived": false, "auto_archive_duration": 1440, "locked": false,
                "archive_timestamp": "2026-10-24T00:00:00.000000+00:00"
            }
        }))
        .unwrap();
        cache.update(&Event::ThreadCreate(Box::new(ThreadCreate(
            channel.clone(),
        ))));
        let mut archived = channel;
        archived.thread_metadata.as_mut().unwrap().archived = true;
        let event = Event::ThreadUpdate(Box::new(ThreadUpdate(archived)));
        assert_eq!(before(&cache, &event).archived, Some(false));
        assert_eq!(
            before(
                &cache,
                &Event::ThreadUpdate(Box::new(ThreadUpdate(
                    serde_json::from_value(json!({ "id": "99", "type": 11, "guild_id": "1" }))
                        .unwrap()
                )))
            )
            .archived,
            None
        );
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait]
    impl GuildHooks for Recorder {
        async fn guild_available(&self, guild: GuildId, name: &str, _: Option<Locale>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("available:{guild}:{name}"));
        }

        async fn guild_removed(&self, guild: GuildId) {
            self.0.lock().unwrap().push(format!("removed:{guild}"));
        }
    }

    fn available(guild: u64, name: &str) -> HookJob {
        HookJob::Available {
            guild: twilight_model::id::Id::new(guild),
            name: name.into(),
            preferred_locale: None,
        }
    }

    #[tokio::test]
    async fn the_newest_hook_of_a_guild_wins_and_pushing_never_waits() {
        let recorder = Arc::new(Recorder::default());
        let queue = Arc::new(HookQueue::default());
        let guild: GuildId = twilight_model::id::Id::new(7);

        // Far more pushes than any bounded channel would hold, with no worker running yet: every push returns at once and the queue keeps one entry per guild.
        for round in 0..100_000 {
            queue.push(available(7, &round.to_string()));
        }
        queue.push(HookJob::Removed { guild });
        assert_eq!(queue.state().pending.len(), 1);

        queue.close();
        run_hook_worker(
            Arc::clone(&recorder) as Arc<dyn GuildHooks>,
            Arc::clone(&queue),
        )
        .await;

        assert_eq!(*recorder.0.lock().unwrap(), ["removed:7"]);
    }

    #[tokio::test]
    async fn the_worker_ends_when_the_queue_is_closed_without_any_shutdown_request() {
        let recorder = Arc::new(Recorder::default());
        let queue = Arc::new(HookQueue::default());
        let worker = tokio::spawn(run_hook_worker(
            Arc::clone(&recorder) as Arc<dyn GuildHooks>,
            Arc::clone(&queue),
        ));
        tokio::task::yield_now().await;
        assert!(!worker.is_finished(), "an open queue keeps the worker");

        queue.close();

        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("a closed, empty queue must end the worker")
            .unwrap();
    }

    #[tokio::test]
    async fn hooks_pushed_right_before_the_queue_closes_are_still_run() {
        let recorder = Arc::new(Recorder::default());
        let queue = Arc::new(HookQueue::default());
        let worker = tokio::spawn(run_hook_worker(
            Arc::clone(&recorder) as Arc<dyn GuildHooks>,
            Arc::clone(&queue),
        ));
        tokio::task::yield_now().await;

        queue.push(available(1, "a"));
        queue.push(available(2, "b"));
        queue.close();
        worker.await.unwrap();

        let mut seen = recorder.0.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, ["available:1:a", "available:2:b"]);
    }

    #[derive(Default)]
    struct Leaves(Mutex<Vec<u64>>);

    impl GuildLeaver for Leaves {
        fn leave(&self, guild: GuildId) {
            self.0.lock().unwrap().push(guild.get());
        }
    }

    #[tokio::test]
    async fn a_guild_that_is_not_allowed_is_left_and_the_others_are_not() {
        use closure_core::testing::{MockApi, StaticSettings};

        let dispatcher = Dispatcher::start(
            Arc::new(ModuleRegistry::new(vec![]).unwrap()),
            Arc::new(StaticSettings::new([])),
            Arc::new(MockApi::default()),
            Arc::new(closure_i18n::TranslatorBuilder::new().unwrap().build()),
            crate::DispatcherConfig::default(),
        )
        .await
        .unwrap();
        let leaves = Arc::new(Leaves::default());
        let queues = Arc::new(HookQueues::new(None));
        let context = ShardContext {
            cache: new_cache(),
            dispatcher,
            hooks: Arc::clone(&queues),
            guild_hooks: Arc::new(NoHooks),
            health: Arc::new(GatewayHealth::new(1)),
            latency: Arc::default(),
            members_intent: false,
            member_request_limit: 0,
            allowed: Some(AllowedGuilds::new(
                HashSet::from([Id::new(1)]),
                Arc::clone(&leaves) as Arc<dyn GuildLeaver>,
            )),
            shutdown: CancellationToken::new(),
        };
        let unavailable = |id: &str| {
            let created: GuildCreate =
                serde_json::from_value(json!({ "id": id, "unavailable": true })).unwrap();
            Event::GuildCreate(Box::new(created))
        };
        let shard = ShardId::new(0, 1);
        handle_event(&context, shard, None, &unavailable("2"));
        handle_event(&context, shard, None, &unavailable("1"));
        handle_event(&context, shard, None, &guild_with(1));
        assert_eq!(*leaves.0.lock().unwrap(), [2]);
        let arrivals = queues.lifecycle.take().jobs.len();
        assert_eq!(arrivals, 1, "the allowed guild is served");
    }

    #[tokio::test]
    async fn the_queue_closes_when_the_last_shard_context_is_dropped() {
        use closure_core::testing::{MockApi, StaticSettings};

        let dispatcher = Dispatcher::start(
            Arc::new(ModuleRegistry::new(vec![]).unwrap()),
            Arc::new(StaticSettings::new([])),
            Arc::new(MockApi::default()),
            Arc::new(closure_i18n::TranslatorBuilder::new().unwrap().build()),
            crate::DispatcherConfig::default(),
        )
        .await
        .unwrap();
        let queues = Arc::new(HookQueues::new(None));
        let context = Arc::new(ShardContext {
            cache: new_cache(),
            dispatcher,
            hooks: Arc::clone(&queues),
            guild_hooks: Arc::new(NoHooks),
            health: Arc::new(GatewayHealth::new(2)),
            latency: Arc::default(),
            members_intent: false,
            member_request_limit: 0,
            allowed: None,
            shutdown: CancellationToken::new(),
        });
        let second = Arc::clone(&context);

        drop(context);
        assert!(
            !queues.lifecycle.take().closed,
            "one shard is still running"
        );
        drop(second);
        assert!(queues.lifecycle.take().closed, "the last shard is gone");
        assert!(queues.lanes.iter().all(|lane| lane.take().closed));
    }

    #[test]
    fn a_guild_always_has_the_same_lane_and_guilds_are_spread_over_them() {
        let queues = HookQueues::new(None);
        assert!(Arc::ptr_eq(
            queues.lane(Id::new(5)),
            queues.lane(Id::new(5))
        ));
        let used: std::collections::HashSet<_> = (1..=64_u64)
            .map(|guild| Arc::as_ptr(queues.lane(Id::new(guild))))
            .collect();
        assert_eq!(used.len(), MESSAGE_LANES);
        assert!(!Arc::ptr_eq(
            queues.lane(Id::new(1)),
            queues.lane(Id::new(2))
        ));
    }

    /// Remembers the id of every event the member-activity hook was given.
    #[derive(Default)]
    struct ActivityLog(Mutex<Vec<u64>>);

    #[async_trait]
    impl GuildHooks for ActivityLog {
        async fn member_activity(&self, event: &ModuleEvent) {
            if let ModuleEvent::MessageDeleted { message_id, .. } = event {
                self.0.lock().unwrap().push(message_id.get());
            }
        }
    }

    fn spool_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("closure-spool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn member_events_past_the_capacity_are_kept_and_played_back_in_order() {
        let dir = spool_dir("order");
        let queue = Arc::new(HookQueue::with_spill(dir.join("lane.jsonl")));
        let capacity = u64::try_from(MESSAGE_QUEUE_CAPACITY).unwrap();
        for id in 1..=capacity + 5 {
            queue.push_member_activity(deleted(1, id));
        }
        assert_eq!(queue.state().messages.len(), MESSAGE_QUEUE_CAPACITY);
        // Once the file holds events, later ones go behind them even if the queue has room.
        queue.state().messages.clear();
        queue.push_member_activity(deleted(1, capacity + 6));
        assert!(queue.state().messages.is_empty());

        let log = Arc::new(ActivityLog::default());
        queue.close();
        run_hook_worker(Arc::clone(&log) as Arc<dyn GuildHooks>, Arc::clone(&queue)).await;

        let rest: Vec<u64> = (capacity + 1..=capacity + 6).collect();
        assert_eq!(
            *log.0.lock().unwrap(),
            rest,
            "the cleared queue held nothing; the file the rest"
        );
        assert!(!queue.state().spilling);
        assert!(!queue.spill.as_ref().unwrap().has_events());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn events_left_in_the_file_by_an_earlier_run_are_played_back_first() {
        let dir = spool_dir("restart");
        {
            let queue = HookQueue::with_spill(dir.join("lane.jsonl"));
            queue.state().spilling = true;
            queue.push_member_activity(deleted(1, 7));
            queue.push_member_activity(deleted(1, 8));
        }
        let queue = Arc::new(HookQueue::with_spill(dir.join("lane.jsonl")));
        assert!(
            queue.state().spilling,
            "a file left behind is worked off first"
        );
        queue.push_member_activity(deleted(1, 9));
        let log = Arc::new(ActivityLog::default());
        queue.close();
        run_hook_worker(Arc::clone(&log) as Arc<dyn GuildHooks>, Arc::clone(&queue)).await;
        assert_eq!(*log.0.lock().unwrap(), [7, 8, 9]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_spool_file_with_nothing_readable_is_dropped_and_not_replayed_for_ever() {
        let dir = spool_dir("garbage");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lane.jsonl"), "not json\n{broken\n").unwrap();
        let queue = Arc::new(HookQueue::with_spill(dir.join("lane.jsonl")));
        let log = Arc::new(ActivityLog::default());
        queue.close();
        tokio::time::timeout(
            Duration::from_secs(5),
            run_hook_worker(Arc::clone(&log) as Arc<dyn GuildHooks>, Arc::clone(&queue)),
        )
        .await
        .expect("the worker ends");
        assert!(log.0.lock().unwrap().is_empty());
        assert!(!queue.spill.as_ref().unwrap().has_events());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn without_a_spool_a_full_queue_drops_the_member_event() {
        let queue = HookQueue::default();
        for id in 1..=u64::try_from(MESSAGE_QUEUE_CAPACITY).unwrap() + 1 {
            queue.push_member_activity(deleted(1, id));
        }
        assert_eq!(queue.state().messages.len(), MESSAGE_QUEUE_CAPACITY);
    }

    #[test]
    fn a_message_backlog_does_not_sit_before_the_guild_hooks() {
        let queues = HookQueues::new(None);
        for id in 1..=10_u64 {
            queues.lane(Id::new(1)).push_message(deleted(1, id));
        }
        queues
            .lifecycle
            .push(HookJob::Removed { guild: Id::new(1) });
        let taken = queues.lifecycle.take();
        assert_eq!((taken.jobs.len(), taken.messages.len()), (1, 0));
    }

    /// Panics on the first message event it is given, and keeps the id of each later one.
    #[derive(Default)]
    struct PanicsOnFirstMessage {
        calls: std::sync::atomic::AtomicUsize,
        kept: Mutex<Vec<u64>>,
    }

    #[async_trait]
    impl GuildHooks for PanicsOnFirstMessage {
        async fn message(&self, event: &mut ModuleEvent) {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(call != 0, "the first message breaks its hook");
            if let ModuleEvent::MessageDeleted { message_id, .. } = event {
                self.kept.lock().unwrap().push(message_id.get());
            }
        }
    }

    #[tokio::test]
    async fn a_message_hook_that_panics_does_not_stop_the_worker() {
        let hooks = Arc::new(PanicsOnFirstMessage::default());
        let queue = Arc::new(HookQueue::default());
        let first = queue
            .push_message(deleted(1, 10))
            .expect("the queue has room");
        let second = queue
            .push_message(deleted(1, 11))
            .expect("the queue has room");
        let worker = tokio::spawn(run_hook_worker(
            Arc::clone(&hooks) as Arc<dyn GuildHooks>,
            Arc::clone(&queue),
        ));

        let wait = Duration::from_secs(5);
        let first = tokio::time::timeout(wait, first)
            .await
            .expect("the lane of the panicking message is released");
        assert!(
            first.is_ok(),
            "a message whose hook panicked is still sent to its lane"
        );
        let second = tokio::time::timeout(wait, second)
            .await
            .expect("the lane of the next message is released");
        assert!(second.is_ok());
        assert_eq!(
            *hooks.kept.lock().unwrap(),
            [11],
            "the worker goes on with the next message"
        );

        queue.close();
        tokio::time::timeout(wait, worker)
            .await
            .expect("the worker ends once the queue is closed")
            .unwrap();
    }

    /// Panics for guild 1 and keeps the guilds of the other arrivals.
    #[derive(Default)]
    struct PanicsInGuildHook {
        kept: Mutex<Vec<u64>>,
    }

    #[async_trait]
    impl GuildHooks for PanicsInGuildHook {
        async fn guild_available(&self, guild: GuildId, _: &str, _: Option<Locale>) {
            assert!(guild.get() != 1, "the guild hook breaks");
            self.kept.lock().unwrap().push(guild.get());
        }
    }

    #[tokio::test]
    async fn a_guild_hook_that_panics_does_not_stop_the_worker() {
        let hooks = Arc::new(PanicsInGuildHook::default());
        let queue = Arc::new(HookQueue::default());
        queue.push(available(1, "boom"));
        queue.push(available(2, "fine"));
        queue.close();
        let worker = tokio::spawn(run_hook_worker(
            Arc::clone(&hooks) as Arc<dyn GuildHooks>,
            Arc::clone(&queue),
        ));

        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .expect("the worker ends once the closed queue is empty")
            .expect("a panicking guild hook does not end the worker");
        assert_eq!(
            *hooks.kept.lock().unwrap(),
            [2],
            "the guild hook after the panic still runs"
        );
    }

    /// Never finishes a message: a worker that takes one stays inside its hook until it is aborted.
    struct HangsOnMessage;

    #[async_trait]
    impl GuildHooks for HangsOnMessage {
        async fn message(&self, _: &mut ModuleEvent) {
            std::future::pending::<()>().await;
        }
    }

    #[tokio::test]
    async fn lanes_waiting_on_an_aborted_worker_are_released() {
        let queue = Arc::new(HookQueue::default());
        let worker = tokio::spawn(run_hook_worker(
            Arc::new(HangsOnMessage) as Arc<dyn GuildHooks>,
            Arc::clone(&queue),
        ));
        let taken = queue
            .push_message(deleted(1, 10))
            .expect("the queue has room");
        // The worker takes the first message and waits in its hook; the second one stays queued.
        tokio::task::yield_now().await;
        let queued = queue
            .push_message(deleted(1, 11))
            .expect("the queue has room");
        worker.abort();

        let wait = Duration::from_secs(5);
        let _ = tokio::time::timeout(wait, taken)
            .await
            .expect("the lane of the message being handled is released");
        let _ = tokio::time::timeout(wait, queued)
            .await
            .expect("the lane of the queued message is released");
        assert!(queue.push_message(deleted(1, 12)).is_none());
    }

    /// Keeps the deleted messages of the guilds it wants, in the order they came.
    #[derive(Default)]
    struct Messages {
        seen: Mutex<Vec<u64>>,
    }

    #[async_trait]
    impl GuildHooks for Messages {
        fn wants_messages(&self, guild: GuildId) -> bool {
            guild == twilight_model::id::Id::new(1)
        }

        async fn message(&self, event: &mut ModuleEvent) {
            if let ModuleEvent::MessageDeleted { message_id, .. } = event {
                self.seen.lock().unwrap().push(message_id.get());
            }
        }
    }

    fn deleted(guild: u64, message: u64) -> ModuleEvent {
        ModuleEvent::MessageDeleted {
            guild_id: twilight_model::id::Id::new(guild),
            channel_id: twilight_model::id::Id::new(5),
            message_id: twilight_model::id::Id::new(message),
        }
    }

    #[tokio::test]
    async fn message_events_of_wanted_guilds_run_in_arrival_order() {
        let hooks = Arc::new(Messages::default());
        let queue = Arc::new(HookQueue::default());
        offer_message(&queue, &*hooks, &deleted(1, 10));
        offer_message(&queue, &*hooks, &deleted(1, 11));
        // Guild 2 does not want messages: the event is never queued, so never cloned either.
        offer_message(&queue, &*hooks, &deleted(2, 12));
        offer_message(&queue, &*hooks, &deleted(1, 13));
        queue.close();

        run_hook_worker(Arc::clone(&hooks) as Arc<dyn GuildHooks>, queue).await;
        assert_eq!(*hooks.seen.lock().unwrap(), [10, 11, 13]);
    }

    #[test]
    fn message_events_past_the_queue_capacity_are_dropped_and_the_first_kept() {
        let queue = HookQueue::default();
        // Snowflakes are never zero, so the ids start at one.
        let capacity = u64::try_from(MESSAGE_QUEUE_CAPACITY).unwrap();
        for id in 1..=capacity + 5 {
            queue.push_message(deleted(1, id));
        }
        let taken = queue.take();
        assert_eq!(taken.messages.len(), MESSAGE_QUEUE_CAPACITY);
        let id_of = |event: &ModuleEvent| match event {
            ModuleEvent::MessageDeleted { message_id, .. } => message_id.get(),
            _ => panic!("only deletions were pushed"),
        };
        assert_eq!(id_of(&taken.messages[0].0), 1);
        assert_eq!(
            id_of(&taken.messages[MESSAGE_QUEUE_CAPACITY - 1].0),
            capacity
        );
    }

    #[test]
    fn the_config_never_prints_the_token() {
        let config = GatewayConfig {
            token: "super-secret".into(),
            intents: Intents::GUILDS,
            latency: Arc::default(),
            member_request_limit: 0,
            member_spool: None,
            allowed_guilds: None,
        };
        assert!(!format!("{config:?}").contains("super-secret"));
    }
}

#[cfg(test)]
mod guild_ready_tests {
    use super::*;

    #[test]
    fn a_guild_needs_the_shard_it_belongs_to_and_not_the_others() {
        let health = GatewayHealth::new(2);
        health.set(ShardId::new(0, 2), true);
        // Shard 0 holds the guilds whose id shifted by 22 is even, shard 1 the odd ones.
        let on_shard_0 = 2_u64 << 22;
        let on_shard_1 = 3_u64 << 22;
        assert!(health.guild_ready(on_shard_0));
        assert!(!health.guild_ready(on_shard_1));
        assert!(!health.all_ready());
        health.set(ShardId::new(1, 2), true);
        assert!(health.guild_ready(on_shard_1));
        assert!(!GatewayHealth::new(0).guild_ready(on_shard_0));
    }
}

#[cfg(test)]
mod leave_retry_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::time::Instant;

    use super::{LEAVE_RETRY_DELAYS, retry_on_error};

    #[tokio::test(start_paused = true)]
    async fn a_leave_that_keeps_failing_is_tried_four_times_over_five_minutes() {
        let attempts = AtomicUsize::new(0);
        let started = Instant::now();

        let result: Result<(), &str> = retry_on_error(&LEAVE_RETRY_DELAYS, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err("unreachable") }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        assert_eq!(started.elapsed(), Duration::from_secs(5 + 30 + 300));
    }

    #[tokio::test(start_paused = true)]
    async fn a_leave_that_succeeds_on_a_retry_is_not_tried_again() {
        let attempts = AtomicUsize::new(0);
        let started = Instant::now();

        let result: Result<(), &str> = retry_on_error(&LEAVE_RETRY_DELAYS, || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            async move { if attempt < 3 { Err("down") } else { Ok(()) } }
        })
        .await;

        assert_eq!(result, Ok(()));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(started.elapsed(), Duration::from_secs(5 + 30));
    }
}
