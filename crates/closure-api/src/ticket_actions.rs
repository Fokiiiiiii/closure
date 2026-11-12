use std::future::Future;
use std::pin::Pin;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use closure_core::event::GuildId;
use closure_core::module::{
    DashboardActionError, DashboardActionRequest, DashboardActionResult, DashboardPublicScope,
    DashboardScope,
};
use closure_core::permission::Actor;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ApiState, config, error::ApiError, messages, tickets};

pub trait TicketActionSink: Send + Sync {
    fn state(
        &self,
        guild: GuildId,
        actor: Actor,
        scope: DashboardScope,
    ) -> Pin<
        Box<dyn Future<Output = Result<DashboardActionResult, DashboardActionError>> + Send + '_>,
    >;

    fn public_state(
        &self,
        guild: GuildId,
        scope: DashboardPublicScope,
    ) -> Pin<
        Box<dyn Future<Output = Result<DashboardActionResult, DashboardActionError>> + Send + '_>,
    >;

    fn perform(
        &self,
        guild: GuildId,
        actor: Actor,
        request: DashboardActionRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<DashboardActionResult, DashboardActionError>> + Send + '_>,
    >;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActionStateQuery {
    cursor: Option<String>,
    q: Option<String>,
    days: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PublicKnowledgeQuery {
    q: Option<String>,
    category: Option<String>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActionBody {
    action: String,
    #[serde(default)]
    data: Value,
}

pub(crate) async fn state(
    State(state): State<ApiState>,
    Path((raw_guild, scope_kind, scope_id)): Path<(u64, String, String)>,
    headers: HeaderMap,
    Query(query): Query<ActionStateQuery>,
) -> Result<Response, ApiError> {
    let (guild, viewer) = tickets::viewer(&state, raw_guild, &headers).await?;
    let scope = state_scope(&scope_kind, &scope_id, query)?;
    let sink = state.ticket_actions.as_ref().ok_or(ApiError::Unavailable)?;
    let result = sink
        .state(guild, viewer.actor, scope)
        .await
        .map_err(|error| map_error(error, false))?;
    let revision = revision(result.revision)?;
    let response = Json(json!({ "state": result.value })).into_response();
    Ok(no_store(config::with_etag(response, revision)))
}

pub(crate) async fn panels(
    State(state): State<ApiState>,
    Path(raw_guild): Path<u64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (guild, viewer) = tickets::viewer(&state, raw_guild, &headers).await?;
    let sink = state.ticket_actions.as_ref().ok_or(ApiError::Unavailable)?;
    let result = sink
        .state(guild, viewer.actor.clone(), DashboardScope::PanelList)
        .await
        .map_err(|error| map_error(error, false))?;
    let entries = result.value["panels"].as_array().ok_or_else(|| {
        ApiError::Internal("the ticket module returned invalid panel data".into())
    })?;
    let panels = entries
        .iter()
        .map(|panel| {
            let id = panel["id"]
                .as_str()
                .filter(|id| valid_scope_id(id))
                .ok_or_else(|| {
                    ApiError::Internal("the ticket module returned an invalid panel ID".into())
                })?;
            let name = panel["name"]
                .as_str()
                .filter(|name| name.len() <= 100 && !name.chars().any(char::is_control))
                .unwrap_or(id);
            Ok(json!({ "id": id, "name": name }))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let response = Json(json!({
        "can_manage_guild": viewer.can_manage,
        "panels": panels,
    }))
    .into_response();
    Ok(no_store(response))
}

pub(crate) async fn public_knowledge_state(
    State(state): State<ApiState>,
    Path(raw_guild): Path<u64>,
    Query(query): Query<PublicKnowledgeQuery>,
) -> Result<Response, ApiError> {
    let guild = config::guild_id(raw_guild)?;
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }
    let query_text = query
        .q
        .map(|query| query.trim().to_owned())
        .filter(|query| !query.is_empty());
    if query_text.as_ref().is_some_and(|query| {
        !(1..=100).contains(&query.chars().count()) || query.chars().any(char::is_control)
    }) {
        return Err(ApiError::BadRequest(
            "q must be 1 to 100 printable characters".into(),
        ));
    }
    let category = query
        .category
        .map(|category| category.trim().to_owned())
        .filter(|category| !category.is_empty());
    if category.as_ref().is_some_and(|category| {
        !(1..=100).contains(&category.chars().count()) || category.chars().any(char::is_control)
    }) {
        return Err(ApiError::BadRequest(
            "category must be 1 to 100 printable characters".into(),
        ));
    }
    let cursor = query
        .cursor
        .map(|cursor| cursor.trim().to_owned())
        .filter(|cursor| !cursor.is_empty());
    if cursor
        .as_ref()
        .is_some_and(|cursor| cursor.len() > 256 || cursor.chars().any(char::is_control))
    {
        return Err(ApiError::BadRequest("cursor is invalid".into()));
    }

    let scope = DashboardPublicScope::KnowledgeIndex {
        query: query_text,
        category,
        cursor,
    };
    let sink = state.ticket_actions.as_ref().ok_or(ApiError::Unavailable)?;
    let result = sink
        .public_state(guild, scope)
        .await
        .map_err(|error| map_error(error, false))?;
    let response = Json(json!({ "state": result.value })).into_response();
    Ok(no_store(response))
}

pub(crate) async fn public_knowledge_article(
    State(state): State<ApiState>,
    Path((raw_guild, slug)): Path<(u64, String)>,
) -> Result<Response, ApiError> {
    let guild = config::guild_id(raw_guild)?;
    if !valid_public_slug(&slug) {
        return Err(ApiError::BadRequest("article slug is invalid".into()));
    }
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }

    let scope = DashboardPublicScope::KnowledgeArticle { slug };
    let sink = state.ticket_actions.as_ref().ok_or(ApiError::Unavailable)?;
    let result = sink
        .public_state(guild, scope)
        .await
        .map_err(|error| map_error(error, false))?;
    let response = Json(json!({ "article": result.value })).into_response();
    Ok(no_store(response))
}

fn valid_public_slug(slug: &str) -> bool {
    (1..=32).contains(&slug.len())
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub(crate) async fn perform(
    State(state): State<ApiState>,
    Path((raw_guild, scope_kind, scope_id)): Path<(u64, String, String)>,
    headers: HeaderMap,
    Json(body): Json<ActionBody>,
) -> Result<Response, ApiError> {
    let (guild, viewer) = tickets::viewer(&state, raw_guild, &headers).await?;
    let scope = scope(&scope_kind, &scope_id)?;
    let expected_revision = config::expected_revision(&headers)?.unwrap_or(0);
    let idempotency_key = messages::request_key(&headers)?
        .ok_or_else(|| ApiError::BadRequest("Idempotency-Key is required".into()))?;
    if !valid_action(&body.action) {
        return Err(ApiError::BadRequest("action is invalid".into()));
    }
    let request = DashboardActionRequest {
        scope,
        action: body.action,
        data: body.data,
        expected_revision,
        idempotency_key,
    };
    let sink = state.ticket_actions.as_ref().ok_or(ApiError::Unavailable)?;
    let result = sink
        .perform(guild, viewer.actor, request)
        .await
        .map_err(|error| map_error(error, true))?;
    let revision = revision(result.revision)?;
    let response = Json(json!({ "result": result.value })).into_response();
    Ok(no_store(config::with_etag(response, revision)))
}

pub(super) fn scope(kind: &str, id: &str) -> Result<DashboardScope, ApiError> {
    match kind {
        "guild" if id == "guild" => Ok(DashboardScope::Guild),
        "panel" if valid_scope_id(id) => Ok(DashboardScope::Panel {
            panel_id: id.to_owned(),
        }),
        "knowledge" if id == "knowledge" => Ok(DashboardScope::Knowledge {
            query: None,
            cursor: None,
        }),
        "tags" if id == "tags" => Ok(DashboardScope::Tags {
            query: None,
            cursor: None,
        }),
        "labels" if id == "labels" => Ok(DashboardScope::TicketLabels { cursor: None }),
        "gallery" if id == "gallery" => Ok(DashboardScope::Gallery),
        "analytics" if id == "analytics" => Ok(DashboardScope::Analytics {
            period_days: 30,
            cursor: None,
        }),
        "integrations" if id == "integrations" => Ok(DashboardScope::IntegrationSecrets),
        "history" => id
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0 && i64::try_from(*id).is_ok())
            .map(|channel_id| DashboardScope::TicketHistory { channel_id })
            .ok_or_else(|| {
                ApiError::BadRequest("the history channel ID must be a positive snowflake".into())
            }),
        "ticket" => id
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0 && i64::try_from(*id).is_ok())
            .map(|channel_id| DashboardScope::Ticket { channel_id })
            .ok_or_else(|| {
                ApiError::BadRequest("the ticket channel ID must be a positive snowflake".into())
            }),
        _ => Err(ApiError::BadRequest(
            "ticket action scope is invalid".into(),
        )),
    }
}

fn state_scope(kind: &str, id: &str, query: ActionStateQuery) -> Result<DashboardScope, ApiError> {
    match scope(kind, id)? {
        DashboardScope::Knowledge { .. } => {
            if query.days.is_some() {
                return Err(ApiError::BadRequest(
                    "days is only valid for analytics state".into(),
                ));
            }
            let search = query
                .q
                .map(|query| query.trim().to_owned())
                .filter(|query| !query.is_empty());
            if search.as_ref().is_some_and(|search| {
                !(1..=100).contains(&search.chars().count()) || search.chars().any(char::is_control)
            }) {
                return Err(ApiError::BadRequest(
                    "q must be 1 to 100 printable characters".into(),
                ));
            }
            let cursor = query_cursor(query.cursor)?;
            Ok(DashboardScope::Knowledge {
                query: search,
                cursor,
            })
        }
        DashboardScope::Tags { .. } => {
            if query.days.is_some() {
                return Err(ApiError::BadRequest(
                    "days is not valid for tag state".into(),
                ));
            }
            let search = query
                .q
                .map(|query| query.trim().to_owned())
                .filter(|query| !query.is_empty());
            if search.as_ref().is_some_and(|search| {
                !(1..=100).contains(&search.chars().count()) || search.chars().any(char::is_control)
            }) {
                return Err(ApiError::BadRequest(
                    "q must be 1 to 100 printable characters".into(),
                ));
            }
            let cursor = tag_cursor(query.cursor)?;
            Ok(DashboardScope::Tags {
                query: search,
                cursor,
            })
        }
        DashboardScope::TicketLabels { .. } => {
            if query.q.is_some() || query.days.is_some() {
                return Err(ApiError::BadRequest(
                    "q and days are not valid for label state".into(),
                ));
            }
            Ok(DashboardScope::TicketLabels {
                cursor: label_cursor(query.cursor)?,
            })
        }
        DashboardScope::Analytics { .. } => {
            if query.q.is_some() {
                return Err(ApiError::BadRequest(
                    "q is not valid for analytics state".into(),
                ));
            }
            let period_days = query.days.unwrap_or(30);
            if !matches!(period_days, 7 | 30 | 90) {
                return Err(ApiError::BadRequest("days must be 7, 30, or 90".into()));
            }
            let cursor = analytics_cursor(query.cursor)?;
            Ok(DashboardScope::Analytics {
                period_days,
                cursor,
            })
        }
        scope => {
            if query.cursor.is_some() || query.q.is_some() || query.days.is_some() {
                return Err(ApiError::BadRequest(
                    "query parameters are only valid for knowledge, tags, labels, or analytics state".into(),
                ));
            }
            Ok(scope)
        }
    }
}

fn label_cursor(cursor: Option<String>) -> Result<Option<String>, ApiError> {
    cursor
        .map(|cursor| {
            let valid = cursor
                .strip_prefix("ticket-label-definition:")
                .is_some_and(|suffix| {
                    suffix.len() == 32
                        && suffix
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                });
            valid
                .then_some(cursor)
                .ok_or_else(|| ApiError::BadRequest("label cursor is invalid".into()))
        })
        .transpose()
}

fn query_cursor(cursor: Option<String>) -> Result<Option<String>, ApiError> {
    cursor
        .map(|cursor| cursor.trim().to_owned())
        .filter(|cursor| !cursor.is_empty())
        .map(|cursor| {
            (1..=32)
                .contains(&cursor.len())
                .then_some(())
                .filter(|()| {
                    cursor.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                })
                .map(|()| cursor.clone())
                .ok_or_else(|| ApiError::BadRequest("cursor is invalid".into()))
        })
        .transpose()
}

fn tag_cursor(cursor: Option<String>) -> Result<Option<String>, ApiError> {
    cursor
        .map(|cursor| cursor.trim().to_owned())
        .filter(|cursor| !cursor.is_empty())
        .map(|cursor| {
            (1..=16)
                .contains(&cursor.len())
                .then_some(())
                .filter(|()| {
                    cursor.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || byte == b'_'
                            || byte == b'-'
                    })
                })
                .map(|()| cursor.clone())
                .ok_or_else(|| ApiError::BadRequest("tag cursor is invalid".into()))
        })
        .transpose()
}

fn analytics_cursor(cursor: Option<String>) -> Result<Option<String>, ApiError> {
    cursor
        .map(|cursor| cursor.trim().to_owned())
        .filter(|cursor| !cursor.is_empty())
        .map(|cursor| {
            (1..=128)
                .contains(&cursor.len())
                .then_some(())
                .filter(|()| {
                    cursor
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
                })
                .map(|()| cursor.clone())
                .ok_or_else(|| ApiError::BadRequest("analytics cursor is invalid".into()))
        })
        .transpose()
}

fn valid_scope_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_action(action: &str) -> bool {
    (1..=64).contains(&action.len())
        && action.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'.'
                || byte == b'_'
                || byte == b'-'
        })
}

fn revision(revision: i64) -> Result<i64, ApiError> {
    (revision >= 0)
        .then_some(revision)
        .ok_or_else(|| ApiError::Internal("the ticket module returned an invalid revision".into()))
}

pub(super) fn map_error(error: DashboardActionError, action: bool) -> ApiError {
    match error {
        DashboardActionError::Unsupported if action => {
            ApiError::BadRequest("unsupported ticket action".into())
        }
        DashboardActionError::Unsupported | DashboardActionError::Unavailable => {
            ApiError::Unavailable
        }
        DashboardActionError::Invalid(message) => ApiError::BadRequest(message),
        DashboardActionError::NotFound => ApiError::NotFound("no such ticket or panel".into()),
        DashboardActionError::Forbidden => {
            ApiError::Forbidden("the actor may not manage this ticket or panel".into())
        }
        DashboardActionError::Conflict => {
            ApiError::Conflict("a ticket operation is already in progress".into())
        }
        DashboardActionError::PreconditionFailed { current } => {
            ApiError::PreconditionFailed { current }
        }
        DashboardActionError::Internal(detail) => ApiError::Internal(detail),
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
