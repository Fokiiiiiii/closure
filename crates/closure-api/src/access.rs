use std::future::Future;
use std::pin::Pin;

use axum::{
    Json,
    extract::{Path, State},
};
use closure_core::event::{GuildId, UserId};
use closure_core::permission::{Actor, GuildPermissions, Level, PermissionKey, Reason};
use serde::Serialize;

use crate::{ApiState, config::guild_id, error::ApiError};

pub trait MemberAccess: Send + Sync {
    fn member(
        &self,
        guild: GuildId,
        user: UserId,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Actor>, ()>> + Send + '_>>;
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
