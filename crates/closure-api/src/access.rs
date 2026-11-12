use std::future::Future;
use std::pin::Pin;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use closure_core::event::{GuildId, UserId};
use closure_core::permission::{Actor, GuildPermissions, Level, PermissionKey, Reason};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{ApiState, config::guild_id, error::ApiError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberSearchResult {
    pub id: String,
    pub name: String,
    pub display_name: String,
}

pub trait MemberAccess: Send + Sync {
    fn member(
        &self,
        guild: GuildId,
        user: UserId,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Actor>, ()>> + Send + '_>>;

    fn search_members(
        &self,
        _guild: GuildId,
        _query: String,
        _limit: u16,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<MemberSearchResult>, ()>> + Send + '_>> {
        Box::pin(async { Err(()) })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MemberSearchQuery {
    scope_kind: String,
    scope_id: String,
    q: String,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Access {
    pub(crate) level: u8,
    level_name: &'static str,
    pub(crate) allowed: bool,
    reason: &'static str,
}

pub(crate) async fn evaluate(
    state: &ApiState,
    guild: GuildId,
    user: UserId,
    permissions: &GuildPermissions,
) -> Result<Access, ApiError> {
    let Some(actor) = state
        .inner
        .access
        .member(guild, user)
        .await
        .map_err(|()| ApiError::Unavailable)?
    else {
        return Ok(Access {
            level: 0,
            level_name: "none",
            allowed: false,
            reason: "not_member",
        });
    };
    let level = permissions.level_of(&actor);
    let key =
        PermissionKey::new("dashboard.edit").map_err(|e| ApiError::Internal(e.to_string()))?;
    let decision = permissions.check(&actor, &key);
    let reason = match decision.reason {
        Reason::Owner => "owner",
        Reason::DeniedUser => "denied_user",
        Reason::DeniedRole(_) => "denied_role",
        Reason::AllowedUser => "allowed_user",
        Reason::AllowedRole(_) => "allowed_role",
        Reason::Level { .. } => "level",
        Reason::UnknownKey => "unknown_key",
    };
    let level_name = if level >= Level::OWNER {
        "owner"
    } else if level >= Level::ADMIN {
        "admin"
    } else if level >= Level::MODERATOR {
        "moderator"
    } else if level >= Level::TRUSTED {
        "trusted"
    } else {
        "general"
    };
    Ok(Access {
        level: level.get(),
        level_name,
        allowed: decision.allowed,
        reason,
    })
}

pub(crate) async fn get_access(
    State(state): State<ApiState>,
    Path((guild, user)): Path<(u64, u64)>,
) -> Result<Json<Access>, ApiError> {
    let guild = guild_id(guild)?;
    let user = guild_id(user)?.cast();
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }
    let (_, current) = crate::config::stored(&state, guild).await?;
    let settings = state.inner.settings.compiler().compile(&current)?;
    Ok(Json(
        evaluate(&state, guild, user, settings.permissions()).await?,
    ))
}

pub(crate) async fn search_members(
    State(state): State<ApiState>,
    Path(raw_guild): Path<u64>,
    headers: HeaderMap,
    Query(query): Query<MemberSearchQuery>,
) -> Result<Response, ApiError> {
    let (guild, viewer) = crate::tickets::viewer(&state, raw_guild, &headers).await?;
    let search = query.q.trim();
    let length = search.chars().count();
    if !(1..=100).contains(&length) || search.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(
            "q must be 1 to 100 printable characters".into(),
        ));
    }
    let scope = crate::ticket_actions::scope(&query.scope_kind, &query.scope_id)?;
    match &scope {
        closure_core::module::DashboardScope::Guild => {
            if !viewer.can_manage {
                let (_, current) = crate::config::stored(&state, guild).await?;
                let settings = state.inner.settings.compiler().compile(&current)?;
                let key = PermissionKey::new("dashboard.edit")
                    .map_err(|error| ApiError::Internal(error.to_string()))?;
                if !settings.permissions().check(&viewer.actor, &key).allowed {
                    return Err(ApiError::Forbidden(
                        "searching server members needs ticket management or dashboard edit access"
                            .into(),
                    ));
                }
            }
        }
        closure_core::module::DashboardScope::Panel { .. } => {
            let sink = state.ticket_actions.as_ref().ok_or(ApiError::Unavailable)?;
            sink.state(guild, viewer.actor.clone(), scope.clone())
                .await
                .map_err(|error| crate::ticket_actions::map_error(error, false))?;
        }
        _ => {
            return Err(ApiError::BadRequest(
                "member search scope must be guild or panel".into(),
            ));
        }
    }
    let limit = u16::try_from(query.limit.unwrap_or(25).clamp(1, 25))
        .map_err(|_| ApiError::BadRequest("member limit is invalid".into()))?;
    let members = state
        .inner
        .access
        .search_members(guild, search.to_owned(), limit)
        .await
        .map_err(|()| ApiError::Unavailable)?;
    let truncated = members.len() == usize::from(limit);
    let mut response = Json(json!({ "members": members, "truncated": truncated })).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// The actor's access level on the dashboard of `guild`, and whether `dashboard.edit` is allowed, from the stored settings.
pub(crate) async fn actor_access(
    state: &ApiState,
    guild: GuildId,
    user: UserId,
    current: &closure_config::GuildOverrides,
) -> Result<Access, ApiError> {
    let settings = state.inner.settings.compiler().compile(current)?;
    evaluate(state, guild, user, settings.permissions()).await
}
