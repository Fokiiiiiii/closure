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

mod auth;
mod config;
mod error;
mod ops;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Router, middleware};
use closure_config::SettingsStore;
use closure_db::Db;
use secrecy::{ExposeSecret, SecretString};
use tower::limit::ConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

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
    pub(crate) render_metrics: Box<dyn Fn() -> String + Send + Sync>,
    pub(crate) token_digest: [u8; 32],
}

/// Everything the handlers need; cheap to clone.
#[derive(Clone)]
pub struct ApiState {
    pub(crate) inner: Arc<Inner>,
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
                render_metrics: Box::new(render_metrics),
                token_digest: auth::digest(token),
            }),
        })
    }
}

/// The complete application.
pub fn router(state: ApiState) -> Router {
    let protected = Router::new()
        .route("/metrics", get(ops::metrics))
        .route("/v1/schema", get(ops::schema))
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
        .layer(ConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests;
