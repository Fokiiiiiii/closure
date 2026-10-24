//! Running the gateway shards.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::event::{Event as ModuleEvent, GuildId};
use closure_core::locale::Locale;
use closure_core::registry::ModuleRegistry;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use twilight_cache_inmemory::{InMemoryCache, ResourceType};
use twilight_gateway::{
    CloseFrame, Config, Event, EventTypeFlags, Intents, Shard, ShardId, StreamExt as _,
    create_recommended,
};
use twilight_http::Client;

use crate::convert;
use crate::dispatcher::Dispatcher;

/// How long a hook may take before the gateway moves on without it.
const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// Called by the gateway before a guild event is dispatched, so persistent state (settings,
/// the database) is up to date by the time a module sees the event.
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
    hooks: Arc<dyn GuildHooks>,
    health: Arc<GatewayHealth>,
    shutdown: CancellationToken,
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
    let context = Arc::new(ShardContext {
        cache,
        dispatcher,
        hooks,
        health: Arc::clone(&health),
        shutdown,
    });
    let tracker = TaskTracker::new();
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
                handle_event(&context, id, latency, event).await;
                if finished {
                    break;
                }
            }
            Err(error) => tracing::warn!(shard = %id, %error, "error receiving gateway event"),
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

async fn handle_event(
    context: &ShardContext,
    id: ShardId,
    latency: Option<Duration>,
    event: Event,
) {
    context.cache.update(&event);
    match &event {
        Event::Ready(_) => context.health.set(id, true),
        Event::GatewayClose(_) => context.health.set(id, false),
        Event::GatewayHeartbeatAck => {
            if let Some(latency) = latency {
                metrics::gauge!("closure_gateway_latency_seconds", "shard" => id.number().to_string())
                    .set(latency.as_secs_f64());
            }
        }
        _ => {}
    }

    let Some(converted) = convert::event(&event) else {
        return;
    };
    run_hooks(context, &converted).await;
    context.dispatcher.dispatch(converted);
}

async fn run_hooks(context: &ShardContext, event: &ModuleEvent) {
    let hook = async {
        match event {
            ModuleEvent::GuildAvailable {
                guild_id,
                name,
                preferred_locale,
            } => {
                context
                    .hooks
                    .guild_available(*guild_id, name, *preferred_locale)
                    .await;
            }
            ModuleEvent::GuildRemoved { guild_id } => context.hooks.guild_removed(*guild_id).await,
            _ => {}
        }
    };
    if tokio::time::timeout(HOOK_TIMEOUT, hook).await.is_err() {
        tracing::warn!(kind = event.kind().as_str(), "guild hook timed out");
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
    fn the_config_never_prints_the_token() {
        let config = GatewayConfig {
            token: "super-secret".into(),
            intents: Intents::GUILDS,
        };
        assert!(!format!("{config:?}").contains("super-secret"));
    }
}
