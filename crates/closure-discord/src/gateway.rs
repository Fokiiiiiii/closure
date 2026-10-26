//! Running the gateway shards.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::event::{Event as ModuleEvent, GuildId};
use closure_core::latency::GatewayLatency;
use closure_core::locale::Locale;
use closure_core::registry::ModuleRegistry;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::error::ReceiveMessageErrorType;
use twilight_gateway::{
    CloseFrame, Config, Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _,
    create_recommended,
};
use twilight_http::Client;

use crate::convert;
use crate::dispatcher::Dispatcher;
use crate::text_commands;

/// How long a hook may take before the gateway moves on without it.
const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// How many message events may wait for their hook. Past it the newest is dropped and logged:
/// keeping them all would let a slow database grow memory without bound.
const MESSAGE_QUEUE_CAPACITY: usize = 10_000;

/// Called by the gateway for guild arrivals and departures.
///
/// Hooks run in order on a background task, right after the event is handed to the dispatcher,
/// so a slow database never stalls reading from the gateway (which would make Discord cut the
/// connection). The price is that a module can see the event a moment before the hook finished.
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

    /// Whether the messages of the guild are wanted now (see [`Self::message`]). Asked for every
    /// message event before it is copied, so it must be a cheap read.
    fn wants_messages(&self, _guild: GuildId) -> bool {
        false
    }

    /// A message event of a guild that [`Self::wants_messages`] said yes to. Hooks of message
    /// events run in the order the events came, before the guild hooks waiting at the same time.
    async fn message(&self, _event: &ModuleEvent) {}
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
}

impl fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("intents", &self.intents)
            .finish_non_exhaustive()
    }
}

/// The intents to request: those the registered modules declare, plus `GUILDS`, without which
/// the bot learns nothing about its servers.
///
/// Intents belong to the connection, not to a guild, so this is the union over all registered
/// modules rather than over the modules enabled somewhere.
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
                    | ResourceType::USER_CURRENT,
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

    /// Completes when every shard has stopped: after shutdown was requested, or because Discord
    /// closed them for good (invalid token, disallowed intents).
    pub async fn closed(&self) {
        self.tracker.wait().await;
    }
}

struct ShardContext {
    cache: Arc<InMemoryCache>,
    dispatcher: Dispatcher,
    hooks: Arc<HookQueue>,
    /// Asked whether a message event is wanted before it is queued.
    guild_hooks: Arc<dyn GuildHooks>,
    health: Arc<GatewayHealth>,
    latency: Arc<GatewayLatency>,
    shutdown: CancellationToken,
}

impl Drop for ShardContext {
    /// Every shard task holds the context, so it is dropped when the last of them has finished,
    /// whether that is because of a shutdown or because Discord closed them for good.
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

/// The newest pending hook of each guild.
///
/// Hooks bring persistent state in line with the guilds the bot is in, so only the latest word
/// about a guild matters: a departure replaces a queued arrival and the other way round. That
/// keeps the queue bounded by the number of guilds, and pushing never waits, so a slow database
/// can never stop a shard from being read (which would get the connection cut).
#[derive(Default)]
struct HookQueue {
    state: Mutex<HookQueueState>,
    wake: Notify,
}

#[derive(Default)]
struct HookQueueState {
    pending: HashMap<GuildId, HookJob>,
    /// Message events in the order they came; every one is kept up to the capacity.
    messages: VecDeque<ModuleEvent>,
    /// Set once no shard can push any more.
    closed: bool,
}

/// What the worker takes from the queue in one go, so that the queue cannot close between the
/// reading of the pending work and the reading of `closed`.
struct Taken {
    jobs: Vec<HookJob>,
    messages: Vec<ModuleEvent>,
    closed: bool,
}

impl HookQueue {
    fn push(&self, job: HookJob) {
        self.state().pending.insert(job.guild(), job);
        self.wake.notify_one();
    }

    /// Queues a message event for [`GuildHooks::message`]. Past the capacity the event is dropped
    /// and logged by its kind alone, never its content.
    fn push_message(&self, event: ModuleEvent) {
        let mut state = self.state();
        if state.messages.len() >= MESSAGE_QUEUE_CAPACITY {
            tracing::warn!(
                kind = event.kind().as_str(),
                "message hooks are behind; a message event was dropped"
            );
            return;
        }
        state.messages.push_back(event);
        self.wake.notify_one();
    }

    /// Declares that nothing will be pushed any more, so the worker may end once it is idle.
    fn close(&self) {
        self.state().closed = true;
        self.wake.notify_one();
    }

    /// Everything pending, and whether the queue is closed. All of it is read under one lock, so
    /// a closed queue that reports no work really is finished.
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

/// Runs queued hooks one at a time. Ends once the queue is closed and empty, so the hooks of
/// events handled while the shards were shutting down are not lost.
async fn run_hook_worker(hooks: Arc<dyn GuildHooks>, queue: Arc<HookQueue>) {
    loop {
        let taken = queue.take();
        if taken.jobs.is_empty() && taken.messages.is_empty() {
            if taken.closed {
                return;
            }
            queue.wake.notified().await;
            continue;
        }
        for event in &taken.messages {
            if tokio::time::timeout(HOOK_TIMEOUT, hooks.message(event))
                .await
                .is_err()
            {
                tracing::warn!(kind = event.kind().as_str(), "message hook timed out");
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
            if tokio::time::timeout(HOOK_TIMEOUT, hook).await.is_err() {
                tracing::warn!(?job, "guild hook timed out");
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

/// Queues a message event when its guild wants messages. The check comes first, so an event of a
/// guild that keeps none is never copied.
fn offer_message(queue: &HookQueue, hooks: &dyn GuildHooks, event: &ModuleEvent) {
    if message_guild(event).is_some_and(|guild| hooks.wants_messages(guild)) {
        queue.push_message(event.clone());
    }
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

/// Connects the recommended number of shards and feeds the cache and the dispatcher until
/// `shutdown` is cancelled.
pub async fn start_gateway(
    http: &Client,
    config: GatewayConfig,
    cache: Arc<InMemoryCache>,
    dispatcher: Dispatcher,
    hooks: Arc<dyn GuildHooks>,
    shutdown: CancellationToken,
) -> Result<GatewayHandle, GatewayError> {
    let gateway = Config::new(config.token, config.intents | Intents::GUILDS);
    let shards: Vec<Shard> = create_recommended(http, gateway, |_, builder| builder.build())
        .await
        .map_err(|error| GatewayError::Start(error.to_string()))?
        .collect();
    tracing::info!(shards = shards.len(), "starting gateway");

    let health = Arc::new(GatewayHealth::new(shards.len()));
    let hook_queue = Arc::new(HookQueue::default());
    let context = Arc::new(ShardContext {
        cache,
        dispatcher,
        hooks: Arc::clone(&hook_queue),
        guild_hooks: Arc::clone(&hooks),
        health: Arc::clone(&health),
        latency: config.latency,
        shutdown,
    });
    let tracker = TaskTracker::new();
    tracker.spawn(run_hook_worker(hooks, hook_queue));
    for shard in shards {
        tracker.spawn(run_shard(shard, Arc::clone(&context)));
    }
    tracker.close();
    Ok(GatewayHandle { tracker, health })
}

async fn run_shard(mut shard: Shard, context: Arc<ShardContext>) {
    let id = shard.id();
    let mut closing = false;
    loop {
        let item = tokio::select! {
            () = context.shutdown.cancelled(), if !closing => {
                closing = true;
                shard.close(CloseFrame::NORMAL);
                continue;
            }
            item = shard.next_event(EventTypeFlags::all()) => item,
        };
        // The stream ends once the connection is closed for good.
        let Some(item) = item else { break };
        match item {
            Ok(event) => {
                // Once shutdown was requested, the close handshake finishing is the end. The
                // stream itself would otherwise reconnect, as it does after any close.
                let finished = closing && matches!(event, Event::GatewayClose(_));
                // `Shard` is not `Sync`, so only plain values cross the await points below.
                let latency = shard.latency().recent().first().copied();
                handle_event(&context, id, latency, &event);
                if finished {
                    break;
                }
            }
            // Never print the error itself: for a payload that fails to parse it contains the
            // whole event, including interaction tokens and member data.
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

/// What the cache holds about what `event` changes. Read before the cache applies the event,
/// because afterwards it only holds the new state.
fn before(cache: &InMemoryCache, event: &Event) -> convert::Before {
    match event {
        Event::MemberUpdate(updated) => convert::Before {
            roles: cache
                .member(updated.guild_id, updated.user.id)
                .map(|member| member.roles().to_vec()),
            ..convert::Before::default()
        },
        Event::ThreadUpdate(updated) => convert::Before {
            archived: cache.channel(updated.0.id).and_then(|channel| {
                channel
                    .thread_metadata
                    .as_ref()
                    .map(|metadata| metadata.archived)
            }),
            ..convert::Before::default()
        },
        _ => convert::Before::default(),
    }
}

fn handle_event(context: &ShardContext, id: ShardId, latency: Option<Duration>, event: &Event) {
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

    for converted in convert::events(event, &before) {
        offer_message(&context.hooks, &*context.guild_hooks, &converted);
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
        context.dispatcher.dispatch_message(converted, |message| {
            text_commands::invoker_permissions(&context.cache, message)
        });
        if let Some(job) = hook {
            context.hooks.push(job);
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

        // Far more pushes than any bounded channel would hold, with no worker running yet:
        // every push returns at once and the queue keeps one entry per guild.
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
        let queue = Arc::new(HookQueue::default());
        let context = Arc::new(ShardContext {
            cache: new_cache(),
            dispatcher,
            hooks: Arc::clone(&queue),
            guild_hooks: Arc::new(NoHooks),
            health: Arc::new(GatewayHealth::new(2)),
            latency: Arc::default(),
            shutdown: CancellationToken::new(),
        });
        let second = Arc::clone(&context);

        drop(context);
        assert!(!queue.take().closed, "one shard is still running");
        drop(second);
        assert!(queue.take().closed, "the last shard is gone");
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

        async fn message(&self, event: &ModuleEvent) {
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
        assert_eq!(id_of(&taken.messages[0]), 1);
        assert_eq!(id_of(&taken.messages[MESSAGE_QUEUE_CAPACITY - 1]), capacity);
    }

    #[test]
    fn the_config_never_prints_the_token() {
        let config = GatewayConfig {
            token: "super-secret".into(),
            intents: Intents::GUILDS,
            latency: Arc::default(),
        };
        assert!(!format!("{config:?}").contains("super-secret"));
    }
}
