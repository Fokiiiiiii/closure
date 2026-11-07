//! The HTTP API.
//!
//! - `GET /healthz`, `GET /readyz`: liveness and readiness, no authentication.
//! - `GET /metrics`, `GET /v1/schema`: operations and tooling, bearer token.
//! - `/v1/guilds/{id}/config`: read and write a guild's configuration as YAML or JSON. Writes
//!   are guarded by `If-Match` with the revision being edited, so two editors cannot silently
//!   overwrite each other.
//!
//! Everything but the two probes needs the admin bearer token. Discord `OAuth2` logins for
//! server administrators belong to the dashboard and are not part of this crate yet.

mod access;
mod auth;
mod config;
mod directory;
mod error;
mod messages;
mod ops;
mod server;
mod stats;

pub use access::MemberAccess;
pub use directory::{Channel, Directory, Guild, GuildDirectory, Role};
pub use messages::{MessageSink, Post, SendFailure, Sent};
pub use server::{ServerLimits, serve};

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Router, middleware};
use closure_config::SettingsStore;
use closure_db::Db;
use secrecy::{ExposeSecret, SecretString};
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnFailure, TraceLayer};

/// Shorter tokens are refused: the token is the only thing protecting configuration writes.
pub const MIN_TOKEN_CHARS: usize = 24;

/// Configuration documents are small; a limit just above the parser's keeps oversized bodies
/// out before they are read.
const MAX_BODY_BYTES: usize = 70 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_REQUESTS: usize = 64;

/// How many gateway shards hold a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatewayStatus {
    pub ready: usize,
    pub total: usize,
}

/// Lets the API report on the gateway without depending on the crate that runs it.
pub trait Readiness: Send + Sync {
    fn gateway(&self) -> GatewayStatus;

    /// Whether the shard of `guild` holds a session. By default all of them must, which is the
    /// answer for a gateway that cannot tell the shards of guilds apart.
    fn guild_ready(&self, _guild: u64) -> bool {
        let status = self.gateway();
        status.total > 0 && status.ready == status.total
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("the admin token must be at least {MIN_TOKEN_CHARS} characters long")]
    WeakToken,
}

pub(crate) struct Inner {
    pub(crate) settings: Arc<SettingsStore>,
    pub(crate) db: Db,
    pub(crate) readiness: Arc<dyn Readiness>,
    pub(crate) directory: Arc<dyn GuildDirectory>,
    pub(crate) access: Arc<dyn MemberAccess>,
    pub(crate) render_metrics: Box<dyn Fn() -> String + Send + Sync>,
    pub(crate) token_digest: [u8; 32],
    pub(crate) failures: auth::FailureLimiter,
    /// Proxies whose `X-Forwarded-For` is believed: a login failure is counted for the client the
    /// proxy names, not for the proxy.
    pub(crate) trusted_proxies: Vec<std::net::IpAddr>,
}

/// Everything the handlers need; cheap to clone.
#[derive(Clone)]
pub struct ApiState {
    pub(crate) inner: Arc<Inner>,
    pub(crate) message_history_days: u32,
    pub(crate) messages: Option<Arc<dyn MessageSink>>,
}

impl fmt::Debug for ApiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiState").finish_non_exhaustive()
    }
}

impl ApiState {
    pub fn new(
        settings: Arc<SettingsStore>,
        db: Db,
        readiness: Arc<dyn Readiness>,
        directory: Arc<dyn GuildDirectory>,
        access: Arc<dyn MemberAccess>,
        render_metrics: impl Fn() -> String + Send + Sync + 'static,
        admin_token: &SecretString,
    ) -> Result<Self, SetupError> {
        let token = admin_token.expose_secret();
        if token.chars().count() < MIN_TOKEN_CHARS {
            return Err(SetupError::WeakToken);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                settings,
                db,
                readiness,
                directory,
                access,
                render_metrics: Box::new(render_metrics),
                token_digest: auth::digest(token),
                failures: auth::FailureLimiter::default(),
                trusted_proxies: Vec::new(),
            }),
            message_history_days: 7,
            messages: None,
        })
    }

    /// Lets the dashboard send messages through the bot.
    #[must_use]
    pub fn with_messages(mut self, sink: Arc<dyn MessageSink>) -> Self {
        self.messages = Some(sink);
        self
    }

    /// Names the reverse proxies in front of the API. For a request that comes from one of them, the
    /// client in `X-Forwarded-For` (the last address the proxy added) is who failed logins are
    /// counted for; without this, everyone behind a proxy shares one allowance.
    #[must_use]
    pub fn with_trusted_proxies(mut self, proxies: Vec<std::net::IpAddr>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.trusted_proxies = proxies;
        }
        self
    }

    #[must_use]
    pub fn with_message_history_days(mut self, days: u32) -> Self {
        self.message_history_days = days;
        self
    }
}

/// The complete application.
pub fn router(state: ApiState) -> Router {
    let protected = Router::new()
        .route("/metrics", get(ops::metrics))
        .route("/v1/schema", get(ops::schema))
        .route("/v1/guilds", get(directory::guilds))
        .route("/v1/guilds/{guild_id}/directory", get(directory::directory))
        .route("/v1/guilds/{guild_id}/stats", get(stats::stats))
        .route(
            "/v1/guilds/{guild_id}/access/{user_id}",
            get(access::get_access),
        )
        .route(
            "/v1/guilds/{guild_id}/config",
            get(config::get_config).put(config::put_config),
        )
        .route(
            "/v1/guilds/{guild_id}/config/validate",
            post(config::validate),
        )
        .route(
            "/v1/guilds/{guild_id}/config/effective",
            get(config::effective),
        )
        .route("/v1/guilds/{guild_id}/config/history", get(config::history))
        .route("/v1/guilds/{guild_id}/messages", post(messages::send))
        .route(
            "/v1/guilds/{guild_id}/scheduled-posts",
            get(messages::scheduled),
        )
        .route(
            "/v1/guilds/{guild_id}/scheduled-posts/{key}",
            delete(messages::unschedule),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_token,
        ));

    Router::new()
        .route("/healthz", get(ops::healthz))
        .route("/readyz", get(ops::readyz))
        .merge(protected)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .layer(
            // A 503 from /readyz while starting up is expected, not an error.
            TraceLayer::new_for_http()
                .on_failure(DefaultOnFailure::new().level(tracing::Level::WARN)),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests;
