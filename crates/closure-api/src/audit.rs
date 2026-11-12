use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiState, error::ApiError, tickets};

const MAX_SCAN: u32 = 10_000;
const MAX_LIMIT: u32 = 2_500;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AuditQuery {
    limit: Option<u32>,
    user_id: Option<String>,
    action_type: Option<String>,
    resource_type: Option<String>,
    outcome: Option<String>,
    date_from: Option<i64>,
    date_to: Option<i64>,
}

pub(crate) async fn events(
    State(state): State<ApiState>,
    Path(raw_guild): Path<u64>,
    headers: HeaderMap,
    Query(query): Query<AuditQuery>,
) -> Result<Response, ApiError> {
    let (guild, viewer) = tickets::viewer(&state, raw_guild, &headers).await?;
    if !viewer.can_manage {
        return Err(ApiError::Forbidden(
            "reading audit events requires ticket management access".into(),
        ));
    }
    if query.date_from.is_some_and(|value| value < 0)
        || query.date_to.is_some_and(|value| value < 0)
        || query
            .date_from
            .zip(query.date_to)
            .is_some_and(|(from, to)| from > to)
    {
        return Err(ApiError::BadRequest("audit date range is invalid".into()));
    }
    if query.user_id.as_deref().is_some_and(|id| {
        id.is_empty()
            || id.len() > 20
            || !id.bytes().all(|byte| byte.is_ascii_digit())
            || id.parse::<u64>().is_err()
            || id.parse::<u64>().is_ok_and(|id| id == 0)
    }) {
        return Err(ApiError::BadRequest("audit user_id is invalid".into()));
    }
    let action_type = query.action_type.as_deref().filter(|value| *value != "all");
    let resource_type = query
        .resource_type
        .as_deref()
        .filter(|value| *value != "all");
    let outcome = query.outcome.as_deref().filter(|value| *value != "all");
    let valid_code = |value: &str| {
        (1..=48).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    };
    if action_type.is_some_and(|value| !valid_code(value))
        || resource_type.is_some_and(|value| !valid_code(value))
        || outcome.is_some_and(|value| !valid_code(value))
    {
        return Err(ApiError::BadRequest("audit filter is invalid".into()));
    }
    let source_matches = action_type.is_none_or(|value| value == "settings_update")
        && resource_type.is_none_or(|value| value == "settings")
        && outcome.is_none_or(|value| value == "success");

    let records = state
        .inner
        .db
        .recent_config_audit_metadata(guild, MAX_SCAN + 1)
        .await?;
    let truncated = records.len() > MAX_SCAN as usize;
    let mut entries = records
        .into_iter()
        .take(MAX_SCAN as usize)
        .filter_map(|record| {
            let timestamp = record.created_at.unix_timestamp();
            let actor_id = record
                .actor
                .map_or_else(|| "system".to_owned(), |actor| actor.to_string());
            let matches = source_matches
                && query
                    .user_id
                    .as_deref()
                    .is_none_or(|user_id| actor_id == user_id)
                && query.date_from.is_none_or(|from| timestamp >= from)
                && query.date_to.is_none_or(|to| timestamp <= to);
            matches.then(|| {
                json!({
                    "timestamp": timestamp,
                    "actor_id": actor_id,
                    "action": "settings_update",
                    "resource": "settings",
                    "outcome": "success",
                })
            })
        })
        .collect::<Vec<Value>>();
    let total = entries.len();
    entries.truncate(query.limit.unwrap_or(MAX_LIMIT).clamp(1, MAX_LIMIT) as usize);

    let mut response = Json(json!({
        "entries": entries,
        "total": total,
        "truncated": truncated,
    }))
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
