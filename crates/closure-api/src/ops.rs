use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::error::ApiError;
use crate::{ApiState, GatewayStatus};

const DATABASE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Liveness: the process is up and serving requests.
pub(crate) async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: the database answers and every gateway shard holds a session. A load balancer or
/// orchestrator should keep traffic away while this is not `200`.
pub(crate) async fn readyz(State(state): State<ApiState>) -> Response {
    let database = tokio::time::timeout(DATABASE_PROBE_TIMEOUT, state.inner.db.ping())
        .await
        .is_ok_and(|result| result.is_ok());
    let GatewayStatus { ready, total } = state.inner.readiness.gateway();
    let gateway = total > 0 && ready == total;

    let status = if database && gateway {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = json!({
        "ready": database && gateway,
        "database": database,
        "gateway": { "ready": ready, "total": total },
    });
    (status, Json(body)).into_response()
}

/// Prometheus text exposition.
pub(crate) async fn metrics(State(state): State<ApiState>) -> Response {
    let mut response = (state.inner.render_metrics)().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    response
}

/// The JSON Schema of the configuration document, including every module's own section.
pub(crate) async fn schema(
    State(state): State<ApiState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = state.inner.settings.compiler().registry();
    Ok(Json(closure_config::document_schema(registry)?))
}
