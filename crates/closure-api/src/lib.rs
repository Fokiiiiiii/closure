//! The HTTP API.
//!
//! - `GET /healthz`, `GET /readyz`: liveness and readiness, no authentication.
//! - `GET /metrics`, `GET /v1/schema`: operations and tooling, bearer token.
//! - `/v1/guilds/{id}/tickets`: read paged open tickets and closed history for dashboard staff.
//! - `/v1/guilds/{id}/config`: read and write a guild's configuration as YAML or JSON. Writes
//!   are guarded by `If-Match` with the revision being edited, so two editors cannot silently
//!   overwrite each other.
//!
//! Everything except the two probes needs the admin bearer token.

mod access;
mod audit;
mod auth;
mod cases;
mod config;
mod directory;
mod error;
mod messages;
pub mod notifications;
pub mod notifications;
mod ops;
mod server;
mod stats;
mod ticket_actions;
mod ticket_gallery;
mod tickets;

pub use access::{MemberAccess, MemberSearchResult};
pub use directory::{Channel, Directory, Guild, GuildDirectory, Role};
pub use messages::{MessageSink, Post, SendFailure, Sent};
pub use server::{ServerLimits, serve};
pub use ticket_actions::TicketActionSink;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Router, middleware};
use closure_config::SettingsStore;
use closure_core::integration_secrets::IntegrationSecretService;
use closure_db::Db;
use secrecy::{ExposeSecret, SecretString};
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::{DefaultOnFailure, TraceLayer};

/// Shorter tokens are refused: the token is the only thing protecting configuration writes.
pub const MIN_TOKEN_CHARS: usize = 24;

/// Configuration documents are small; the limit sits just above the parser's to reject oversized bodies early.
const MAX_BODY_BYTES: usize = 70 * 1024;
/// Covers the wait for a concurrency slot as well as the handling.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A readiness answer that takes longer than this is no answer.
const READY_TIMEOUT: Duration = Duration::from_secs(2);
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
    pub(crate) account_vault: Option<Arc<IntegrationSecretService>>,
}

/// Everything the handlers need; cheap to clone.
#[derive(Clone)]
pub struct ApiState {
    pub(crate) inner: Arc<Inner>,
    pub(crate) message_history_days: u32,
    pub(crate) messages: Option<Arc<dyn MessageSink>>,
    pub(crate) ticket_actions: Option<Arc<dyn TicketActionSink>>,
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
                account_vault: None,
            }),
            message_history_days: 7,
            messages: None,
            ticket_actions: None,
        })
    }

    /// Lets the dashboard send messages through the bot.
    #[must_use]
    pub fn with_messages(mut self, sink: Arc<dyn MessageSink>) -> Self {
        self.messages = Some(sink);
        self
    }

    /// Lets the dashboard call ticket operations through the ticket module.
    #[must_use]
    pub fn with_ticket_actions(mut self, sink: Arc<dyn TicketActionSink>) -> Self {
        self.ticket_actions = Some(sink);
        self
    }

    /// Supplies the account-scoped encrypted email vault.
    #[must_use]
    pub fn with_account_vault(mut self, vault: Option<Arc<IntegrationSecretService>>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.account_vault = vault;
        }
        self
    }

    /// Trusted reverse proxies: for their requests, failed logins count the client in `X-Forwarded-For`.
    /// Without this, every client behind a proxy shares one login allowance.
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
            "/v1/guilds/{guild_id}/members/search",
            get(access::search_members),
        )
        .route(
            "/v1/guilds/{guild_id}/public/ticket-knowledge",
            get(ticket_actions::public_knowledge_state),
        )
        .route(
            "/v1/guilds/{guild_id}/public/ticket-knowledge/{slug}",
            get(ticket_actions::public_knowledge_article),
        )
        .route(
            "/v1/guilds/{guild_id}/ticket-gallery/templates",
            get(ticket_gallery::templates),
        )
        .route(
            "/v1/guilds/{guild_id}/ticket-gallery/templates/{template_id}",
            get(ticket_gallery::template),
        )
        .route(
            "/v1/guilds/{guild_id}/ticket-gallery/templates/{template_id}/imports",
            post(ticket_gallery::record_import),
        )
        .route(
            "/v1/guilds/{guild_id}/ticket-gallery/submissions",
            get(ticket_gallery::submissions).post(ticket_gallery::submit),
        )
        .route(
            "/v1/guilds/{guild_id}/ticket-gallery/submissions/{submission_id}/actions",
            post(ticket_gallery::submission_action),
        )
        .route(
            "/v1/ticket-gallery/templates/{template_id}/review",
            post(ticket_gallery::review),
        )
        .route("/v1/guilds/{guild_id}/audit-events", get(audit::events))
        .route(
            "/v1/guilds/{guild_id}/ticket-actions/panels",
            get(ticket_actions::panels),
        )
        .route(
            "/v1/guilds/{guild_id}/ticket-actions/{scope_kind}/{scope_id}",
            get(ticket_actions::state).post(ticket_actions::perform),
        )
        .route("/v1/notifications", get(notifications::list))
        .route("/v1/notifications/actions", post(notifications::action))
        .route("/v1/notifications", get(notifications::list))
        .route("/v1/notifications/actions", post(notifications::action))
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
            "/v1/guilds/{guild_id}/cases",
            get(cases::list).post(cases::add_note),
        )
        .route("/v1/guilds/{guild_id}/cases/summary", get(cases::summary))
        .route("/v1/guilds/{guild_id}/cases/export", get(cases::export))
        .route(
            "/v1/guilds/{guild_id}/cases/{number}",
            get(cases::get).patch(cases::change),
        )
        .route("/v1/guilds/{guild_id}/tickets", get(tickets::list))
        .route(
            "/v1/guilds/{guild_id}/tickets/{channel_id}",
            get(tickets::get),
        )
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

    // Outside the business concurrency limit: a saturated API must not stall the liveness probe,
    // or an orchestrator restarts a healthy process.
    let probes = Router::new().route("/healthz", get(ops::healthz)).route(
        "/readyz",
        get(ops::readyz).layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            READY_TIMEOUT,
        )),
    );
    let business = protected
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ));
    Router::new()
        .merge(probes)
        .merge(business)
        .layer(
            // A 503 from /readyz while starting up is expected, not an error.
            TraceLayer::new_for_http()
                .on_failure(DefaultOnFailure::new().level(tracing::Level::WARN)),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests;
