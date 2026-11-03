use axum::{
    Json,
    extract::{Path, State},
};
use closure_core::event::GuildId;
use serde::Serialize;

use crate::{ApiState, config::guild_id, error::ApiError};

#[derive(Debug, Serialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub parent_id: Option<String>,
    pub position: i64,
}
#[derive(Debug, Serialize)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub color: u32,
    pub position: i64,
    pub managed: bool,
}
#[derive(Debug, Serialize)]
pub struct Directory {
    pub channels: Vec<Channel>,
    pub roles: Vec<Role>,
}

pub trait GuildDirectory: Send + Sync {
    fn guild(&self, guild: GuildId) -> Option<Guild>;
    fn directory(&self, guild: GuildId) -> Option<Directory>;
}

pub(crate) async fn guilds(State(state): State<ApiState>) -> Result<Json<Vec<Guild>>, ApiError> {
    let status = state.inner.readiness.gateway();
    if status.total == 0 || status.ready != status.total {
        return Err(ApiError::Unavailable);
    }
    let ids = state.inner.db.active_guilds().await?;
    let guilds = ids
        .into_iter()
        .filter_map(|id| state.inner.directory.guild(id))
        .collect();
    Ok(Json(guilds))
}

pub(crate) async fn directory(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
) -> Result<Json<Directory>, ApiError> {
    let guild = guild_id(raw)?;
    let status = state.inner.readiness.gateway();
    if status.total == 0 || status.ready != status.total {
        return Err(ApiError::Unavailable);
    }
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }
    state
        .inner
        .directory
        .directory(guild)
        .map(Json)
        .ok_or(ApiError::Unavailable)
}
