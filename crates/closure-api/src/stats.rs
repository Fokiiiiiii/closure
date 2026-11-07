use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderValue, header},
    response::{IntoResponse, Response},
};
use closure_core::timezone::Timezone;
use serde::Deserialize;
use serde_json::json;

use crate::{
    ApiState,
    config::{guild_id, stored},
    error::ApiError,
};

#[derive(Deserialize)]
pub(crate) struct StatsQuery {
    days: Option<String>,
}

pub(crate) async fn stats(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
    Query(query): Query<StatsQuery>,
) -> Result<Response, ApiError> {
    let guild = guild_id(raw)?;
    let days = query
        .days
        .as_deref()
        .unwrap_or("7")
        .parse::<i32>()
        .ok()
        .filter(|days| (1..=30).contains(days))
        .ok_or_else(|| ApiError::BadRequest("days must be an integer from 1 to 30".into()))?;
    let (revision, overrides) = stored(&state, guild).await?;
    // The effective timezone, with what the bot's configuration file gives under the server's own.
    let settings = state.inner.settings.compiler().compile(&overrides)?;
    let timezone = overrides
        .timezone
        .as_deref()
        .or(settings.effective.timezone.as_deref())
        .unwrap_or("UTC");
    let offset = Timezone::parse(timezone)
        .map_err(|error| ApiError::Internal(error.to_string()))?
        .offset_seconds();
    let mut result = state.inner.db.guild_stats(guild, days, offset).await?;
    result["timezone"] = json!(timezone);
    result["days"] = json!(days);
    result["members"]["total"] = json!(if revision == 0 {
        None
    } else {
        state.inner.directory.member_count(guild).await
    });
    result["messages"]["retention_days"] = json!(state.message_history_days);
    let mut response = Json(result).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
