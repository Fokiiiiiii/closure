//! Running the gateway shards.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::event::{Event as ModuleEvent, GuildId};
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

/// How long a hook may take before the gateway moves on without it.
const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

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
    health: Arc<GatewayHealth>,
    shutdown: CancellationToken,
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
    pending: Mutex<HashMap<GuildId, HookJob>>,
    wake: Notify,
}

impl HookQueue {
    fn push(&self, job: HookJob) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(job.guild(), job);
        self.wake.notify_one();
    }

    fn take(&self) -> Vec<HookJob> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner))
            .into_values()
            .collect()
    }
}

/// Runs queued hooks one at a time. After `shutdown` it finishes what is queued and ends.
async fn run_hook_worker(
    hooks: Arc<dyn GuildHooks>,
    queue: Arc<HookQueue>,
    shutdown: CancellationToken,
) {
    loop {
        let jobs = queue.take();
        if jobs.is_empty() {
            if shutdown.is_cancelled() {
                return;
            }
            tokio::select! {
                () = queue.wake.notified() => {}
                () = shutdown.cancelled() => {}
            }
            continue;
        }
        for job in jobs {
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
        health: Arc::clone(&health),
        shutdown,
    });
    let tracker = TaskTracker::new();
    tracker.spawn(run_hook_worker(hooks, hook_queue, context.shutdown.clone()));
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

fn handle_event(context: &ShardContext, id: ShardId, latency: Option<Duration>, event: &Event) {
    context.cache.update(event);
    if let Some(ready) = readiness_change(event) {
        context.health.set(id, ready);
    }
    if matches!(event, Event::GatewayHeartbeatAck)
        && let Some(latency) = latency
    {
        metrics::gauge!("closure_gateway_latency_seconds", "shard" => id.number().to_string())
            .set(latency.as_secs_f64());
    }

    let Some(converted) = convert::event(event) else {
        return;
    };
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
    context.dispatcher.dispatch(converted);
    if let Some(job) = hook {
        context.hooks.push(job);
    }
}

#[cfg(test)]
mod tests {
    use closure_core::module::{Module, ModuleId, ModuleManifest};
    use closure_core::testing::StubModule;

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

    #[tokio::test]
    async fn the_newest_hook_of_a_guild_wins_and_pushing_never_waits() {
        use std::sync::Mutex;

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

        let recorder = Arc::new(Recorder::default());
        let queue = Arc::new(HookQueue::default());
        let shutdown = CancellationToken::new();
        let guild: GuildId = twilight_model::id::Id::new(7);

        // Far more pushes than any bounded channel would hold, with no worker running yet:
        // every push returns at once and the queue keeps one entry per guild.
        for round in 0..100_000 {
            queue.push(HookJob::Available {
                guild,
                name: round.to_string(),
                preferred_locale: None,
            });
        }
        queue.push(HookJob::Removed { guild });
        assert_eq!(queue.pending.lock().unwrap().len(), 1);

        shutdown.cancel();
        run_hook_worker(
            Arc::clone(&recorder) as Arc<dyn GuildHooks>,
            Arc::clone(&queue),
            shutdown,
        )
        .await;

        assert_eq!(*recorder.0.lock().unwrap(), ["removed:7"]);
    }

    #[test]
    fn the_config_never_prints_the_token() {
        let config = GatewayConfig {
            token: "super-secret".into(),
            intents: Intents::GUILDS,
        };
        assert!(!format!("{config:?}").contains("super-secret"));
    }
}
