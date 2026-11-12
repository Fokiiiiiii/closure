use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use closure_core::capability::{Capability, CapabilitySet};
use closure_core::event::GuildId;
use closure_core::module::ModuleId;
use closure_core::permission::{Actor, PermissionKey};
use closure_core::storage::{Entry, StorageBackend, StorageError, Store};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::ApiState;
use crate::config::{actor, guild_id, stored};
use crate::error::ApiError;

const OPEN_PREFIX: &str = "ticket:";
const HISTORY_PREFIX: &str = "ticket-history:";
const HISTORY_ID_PREFIX: &str = "ticket-history-id:";
const HISTORY_CONTENT_PREFIX: &str = "ticket-history-content:";
const HISTORY_SPAN_PREFIX: &str = "ticket-history-span:";
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 100;
const SCAN_BATCH: usize = 100;
const MAX_SCAN: usize = 1000;
const TRANSCRIPT_PAGE_SIZE: usize = 4;
const CURSOR_SENTINEL: &str = "99999999999999999999";

#[derive(Deserialize)]
pub(crate) struct ListQuery {
    state: Option<String>,
    limit: Option<usize>,
    before: Option<String>,
    ticket_id: Option<String>,
    username: Option<String>,
    user_id: Option<String>,
    closed_by: Option<String>,
    panel: Option<String>,
    rating: Option<u8>,
    claimed_by: Option<String>,
    close_reason: Option<String>,
    #[serde(default, rename = "label")]
    labels: Vec<String>,
    include_total: Option<bool>,
}

impl ListQuery {
    fn has_closed_filters(&self) -> bool {
        self.ticket_id.is_some()
            || self.username.is_some()
            || self.user_id.is_some()
            || self.closed_by.is_some()
            || self.panel.is_some()
            || self.rating.is_some()
            || self.claimed_by.is_some()
            || self.close_reason.is_some()
            || !self.labels.is_empty()
            || self.include_total.unwrap_or(false)
    }
}

#[derive(Default)]
struct ClosedFilters {
    ticket_number: Option<i64>,
    username_openers: Option<HashSet<u64>>,
    opener: Option<u64>,
    closer: Option<u64>,
    panel: Option<String>,
    rating: Option<u8>,
    claimed_by: Option<u64>,
    reason: Option<String>,
    labels: Vec<String>,
}

impl ClosedFilters {
    fn matches(&self, history: &TicketHistory) -> bool {
        self.ticket_number
            .map_or(true, |number| history.number == number)
            && self
                .username_openers
                .as_ref()
                .map_or(true, |openers| openers.contains(&history.opener))
            && self.opener.map_or(true, |opener| history.opener == opener)
            && self
                .closer
                .map_or(true, |closer| history.closer == Some(closer))
            && self.panel.as_deref().map_or(true, |panel| {
                panel_id(history.panel.as_ref()) == Some(panel)
            })
            && self.rating.map_or(true, |rating| {
                history.rating.as_ref().map(|value| value.score) == Some(rating)
            })
            && self
                .claimed_by
                .map_or(true, |claimed_by| history.claimed_by == Some(claimed_by))
            && self.reason.as_deref().map_or(true, |query| {
                history
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.to_lowercase().contains(query))
            })
            && (self.labels.is_empty()
                || history.labels.iter().any(|label| {
                    self.labels
                        .iter()
                        .any(|selected| selected.eq_ignore_ascii_case(label))
                }))
    }
}

#[derive(Deserialize)]
pub(crate) struct TranscriptQuery {
    start: Option<usize>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct OpenTicket {
    number: i64,
    opener: u64,
    #[serde(default)]
    claimed_by: Option<u64>,
    created: i64,
    #[serde(default)]
    closing: Option<i64>,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    staff_roles: Option<Vec<u64>>,
    #[serde(default)]
    panel: Option<Value>,
    #[serde(default)]
    labels: Vec<String>,
}

#[derive(Deserialize)]
struct TicketRating {
    score: u8,
}

#[derive(Default, Deserialize)]
struct HistoryStaffAccess {
    #[serde(default)]
    teams: Vec<HistoryStaffTeam>,
}

#[derive(Default, Deserialize)]
struct HistoryStaffTeam {
    #[serde(default)]
    roles: Vec<u64>,
    #[serde(default)]
    users: Vec<u64>,
}

#[derive(Deserialize)]
struct TicketHistory {
    channel_id: u64,
    number: i64,
    opener: u64,
    #[serde(default)]
    opener_erased: bool,
    #[serde(default)]
    claimed_by: Option<u64>,
    created: i64,
    #[serde(default)]
    closed: Option<i64>,
    #[serde(default)]
    closer: Option<u64>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    panel: Option<Value>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    rating: Option<TicketRating>,
    #[serde(default)]
    staff_access: Option<HistoryStaffAccess>,
    #[serde(default)]
    staff_roles: Vec<u64>,
    status: String,
    #[serde(default)]
    privacy_generation: u64,
    #[serde(default)]
    transcript_attempt: Option<i64>,
    #[serde(default)]
    transcript_span_pages: usize,
    #[serde(default)]
    transcript_pages: usize,
    #[serde(default)]
    transcript_truncated: bool,
}

#[derive(Deserialize)]
struct TranscriptSpan {
    start: u32,
    end: u32,
    part: TranscriptSpanPart,
    #[serde(default)]
    rich: Option<Value>,
}

#[derive(Deserialize)]
enum TranscriptSpanPart {
    Message { message_id: u64, author_id: u64 },
    Opener { user_id: u64 },
    FormAnswers { user_id: u64 },
}

pub(super) struct Viewer {
    pub(super) actor: Actor,
    pub(super) can_manage: bool,
    default_staff_roles: Vec<u64>,
}

pub(crate) async fn list(
    State(state): State<ApiState>,
    Path(raw_guild): Path<u64>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let (guild, viewer) = viewer(&state, raw_guild, &headers).await?;
    let limit = query.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let store = ticket_store(&state, guild)?;
    match query.state.as_deref() {
        Some("open") => {
            if query.has_closed_filters() {
                return Err(ApiError::BadRequest(
                    "transcript filters are only valid for closed tickets".into(),
                ));
            }
            let before = query.before.as_deref().map(parse_snowflake).transpose()?;
            let (tickets, next_before) = list_open(&store, &viewer, before, limit).await?;
            Ok(Json(json!({
                "tickets": tickets,
                "next_before": next_before,
            })))
        }
        Some("closed") => {
            let before = query
                .before
                .as_deref()
                .map(parse_ticket_number)
                .transpose()?;
            let (filters, username_truncated) = closed_filters(&state, guild, &query).await?;
            let include_total = query.include_total.unwrap_or(false);
            let (tickets, next_before, total, mut total_complete) =
                list_closed(&store, &viewer, before, limit, &filters, include_total).await?;
            if include_total && username_truncated {
                total_complete = Some(false);
            }
            Ok(Json(json!({
                "tickets": tickets,
                "next_before": next_before,
                "total": total,
                "total_complete": total_complete,
                "username_truncated": username_truncated,
            })))
        }
        _ => Err(ApiError::BadRequest("state must be open or closed".into())),
    }
}

async fn closed_filters(
    state: &ApiState,
    guild: GuildId,
    query: &ListQuery,
) -> Result<(ClosedFilters, bool), ApiError> {
    let username = query
        .username
        .as_deref()
        .map(str::trim)
        .filter(|username| !username.is_empty());
    if username.is_some_and(|username| {
        username.chars().count() > 100 || username.chars().any(char::is_control)
    }) {
        return Err(ApiError::BadRequest(
            "username must be 1 to 100 printable characters".into(),
        ));
    }
    let (username_openers, username_truncated) = if let Some(username) = username {
        let members = state
            .inner
            .access
            .search_members(guild, username.to_owned(), 25)
            .await
            .map_err(|()| ApiError::Unavailable)?;
        let truncated = members.len() == 25;
        let openers = members
            .into_iter()
            .map(|member| parse_snowflake(&member.id))
            .collect::<Result<HashSet<_>, _>>()?;
        (Some(openers), truncated)
    } else {
        (None, false)
    };
    let ticket_number = query
        .ticket_id
        .as_deref()
        .map(parse_ticket_number)
        .transpose()?;
    let opener = query.user_id.as_deref().map(parse_snowflake).transpose()?;
    let closer = query
        .closed_by
        .as_deref()
        .map(parse_snowflake)
        .transpose()?;
    let claimed_by = query
        .claimed_by
        .as_deref()
        .map(parse_snowflake)
        .transpose()?;
    if query
        .rating
        .is_some_and(|rating| !(1..=5).contains(&rating))
    {
        return Err(ApiError::BadRequest("rating must be from 1 to 5".into()));
    }
    let panel = query
        .panel
        .as_deref()
        .map(str::trim)
        .filter(|panel| !panel.is_empty());
    if panel.is_some_and(|panel| {
        !(1..=32).contains(&panel.len())
            || !panel
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    }) {
        return Err(ApiError::BadRequest("panel filter is invalid".into()));
    }
    let reason = query
        .close_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty());
    if reason
        .is_some_and(|reason| reason.chars().count() > 100 || reason.chars().any(char::is_control))
    {
        return Err(ApiError::BadRequest(
            "close_reason must be 1 to 100 printable characters".into(),
        ));
    }
    if query.labels.len() > 25
        || query.labels.iter().any(|label| {
            let label = label.trim();
            !(1..=100).contains(&label.chars().count()) || label.chars().any(char::is_control)
        })
    {
        return Err(ApiError::BadRequest("label filter is invalid".into()));
    }
    let mut labels = Vec::new();
    for label in &query.labels {
        let label = label.trim().to_owned();
        if !labels
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&label))
        {
            labels.push(label);
        }
    }
    Ok((
        ClosedFilters {
            ticket_number,
            username_openers,
            opener,
            closer,
            panel: panel.map(str::to_owned),
            rating: query.rating,
            claimed_by,
            reason: reason.map(|reason| reason.to_lowercase()),
            labels,
        },
        username_truncated,
    ))
}

pub(crate) async fn get(
    State(state): State<ApiState>,
    Path((raw_guild, raw_channel)): Path<(u64, u64)>,
    headers: HeaderMap,
    Query(query): Query<TranscriptQuery>,
) -> Result<Json<Value>, ApiError> {
    let (guild, viewer) = viewer(&state, raw_guild, &headers).await?;
    let channel = checked_snowflake(raw_channel, "channel")?;
    let start = query.start.unwrap_or(0);
    let limit = query
        .limit
        .unwrap_or(TRANSCRIPT_PAGE_SIZE)
        .clamp(1, TRANSCRIPT_PAGE_SIZE);
    let store = ticket_store(&state, guild)?;
    let open_key = format!("{OPEN_PREFIX}{channel}");
    if let Some(entry) = store
        .get::<OpenTicket>(&open_key)
        .await
        .map_err(storage_error)?
        .filter(|entry| !entry.closed)
    {
        let staff_roles = open_staff_roles(&entry, &viewer.default_staff_roles);
        if !can_view(&viewer, &staff_roles) {
            return Err(no_ticket());
        }
        if start > 0 {
            return Err(ApiError::BadRequest(
                "an open ticket has no transcript".into(),
            ));
        }
        let state = if entry.closing.is_some() {
            "closing"
        } else {
            "open"
        };
        return Ok(Json(ticket_json(TicketView {
            channel,
            number: entry.number,
            opener: entry.opener,
            claimed_by: entry.claimed_by,
            created: entry.created,
            state,
            closed_at: None,
            closer_id: None,
            reason: None,
            revision: None,
            panel_id: panel_id(entry.panel.as_ref()),
            labels: entry.labels.clone(),
            rating: None,
            has_transcript: false,
            transcript: Some(json!({
                "pages": [],
                "next_page": null,
                "truncated": false,
            })),
        })));
    }

    let pointer_key = format!("{HISTORY_ID_PREFIX}{channel}");
    let Some(history_key) = store
        .get::<String>(&pointer_key)
        .await
        .map_err(storage_error)?
    else {
        return Err(no_ticket());
    };
    if !history_key.starts_with(HISTORY_PREFIX) || !history_key.ends_with(&format!(":{channel}")) {
        return Err(ApiError::Internal("invalid ticket history index".into()));
    }
    let Some(history_entry) = store.entry(&history_key).await.map_err(storage_error)? else {
        return Err(no_ticket());
    };
    let history_revision = history_entry.revision;
    let history = decode::<TicketHistory>(history_entry)?;
    if history.channel_id != channel || history.status != "closed" {
        return Err(no_ticket());
    }
    if history.number <= 0 || history.number == i64::MAX {
        return Err(ApiError::Internal("invalid ticket history number".into()));
    }
    let expected_key = format!(
        "{HISTORY_PREFIX}{:019}:{channel}",
        i64::MAX - history.number
    );
    if history_key != expected_key {
        return Err(ApiError::Internal(
            "ticket history key does not match its record".into(),
        ));
    }
    if !history_privacy_generation_current(&store, &history_key, history.privacy_generation).await?
    {
        return Err(no_ticket());
    }
    if !can_read_history(&viewer, &history) {
        return Err(no_ticket());
    }
    let transcript = transcript(&store, channel, &history, start, limit).await?;
    if !history_privacy_generation_current(&store, &history_key, history.privacy_generation).await?
    {
        return Err(no_ticket());
    }
    Ok(Json(ticket_json(TicketView {
        channel,
        number: history.number,
        opener: history.opener,
        claimed_by: history.claimed_by,
        created: history.created,
        state: "closed",
        closed_at: history.closed,
        closer_id: history.closer,
        reason: history.reason.as_deref(),
        revision: Some(history_revision),
        panel_id: panel_id(history.panel.as_ref()),
        labels: history.labels.clone(),
        rating: history.rating.as_ref().map(|rating| rating.score),
        has_transcript: history.transcript_pages > 0,
        transcript: Some(transcript),
    })))
}

pub(super) async fn viewer(
    state: &ApiState,
    raw_guild: u64,
    headers: &HeaderMap,
) -> Result<(GuildId, Viewer), ApiError> {
    let guild = guild_id(raw_guild)?;
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }
    let user = actor(headers)?
        .ok_or_else(|| ApiError::Forbidden("a verified dashboard actor is required".into()))?;
    let actor = state
        .inner
        .access
        .member(guild, user)
        .await
        .map_err(|()| ApiError::Unavailable)?
        .ok_or_else(|| ApiError::Forbidden("the actor is not a member of this server".into()))?;
    let (_, current) = stored(state, guild).await?;
    let settings = state.inner.settings.compiler().compile(&current)?;
    let key = PermissionKey::new("tickets.manage")
        .map_err(|error| ApiError::Internal(error.to_string()))?;
    let can_manage = settings.permissions().check(&actor, &key).allowed;
    let ticket_module =
        ModuleId::new("tickets").map_err(|error| ApiError::Internal(error.to_string()))?;
    let default_staff_roles = settings
        .module(&ticket_module)
        .and_then(|module| module.config.get("staff_roles"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(staff_role_id)
        .collect();
    Ok((
        guild,
        Viewer {
            actor,
            can_manage,
            default_staff_roles,
        },
    ))
}

fn ticket_store(state: &ApiState, guild: GuildId) -> Result<Store, ApiError> {
    let module = ModuleId::new("tickets").map_err(|error| ApiError::Internal(error.to_string()))?;
    let granted: CapabilitySet = [Capability::Storage].into_iter().collect();
    let backend: Arc<dyn StorageBackend> = Arc::new(state.inner.db.clone());
    Ok(Store::new(backend, &granted, module, guild))
}

async fn history_privacy_generation_current(
    store: &Store,
    key: &str,
    generation: u64,
) -> Result<bool, ApiError> {
    let Some(history) = store
        .get::<TicketHistory>(key)
        .await
        .map_err(storage_error)?
    else {
        return Ok(false);
    };
    Ok(history.status == "closed" && history.privacy_generation == generation)
}

fn can_view(viewer: &Viewer, staff_roles: &[u64]) -> bool {
    viewer.can_manage
        || viewer
            .actor
            .roles
            .iter()
            .any(|role| staff_roles.contains(&role.get()))
}

fn can_view_history(viewer: &Viewer, history: &TicketHistory) -> bool {
    if viewer.can_manage {
        return true;
    }
    if let Some(snapshot) = &history.staff_access {
        let user = viewer.actor.user.get();
        return snapshot.teams.iter().any(|team| {
            team.users.contains(&user)
                || viewer
                    .actor
                    .roles
                    .iter()
                    .any(|role| team.roles.contains(&role.get()))
        });
    }
    can_view(viewer, &history.staff_roles)
}

fn can_read_history(viewer: &Viewer, history: &TicketHistory) -> bool {
    can_view_history(viewer, history)
        || (history.status == "closed"
            && !history.opener_erased
            && viewer.actor.user.get() == history.opener)
}

fn open_staff_roles(ticket: &OpenTicket, defaults: &[u64]) -> Vec<u64> {
    if let Some(staff_roles) = &ticket.staff_roles {
        return staff_roles.clone();
    }
    ticket.panel.as_ref().map_or_else(
        || defaults.to_vec(),
        |panel| {
            panel
                .get("staff_roles")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(staff_role_id)
                .collect()
        },
    )
}

fn staff_role_id(value: &Value) -> Option<u64> {
    let id = value
        .as_u64()
        .or_else(|| value.as_str()?.parse::<u64>().ok())?;
    (id > 0 && i64::try_from(id).is_ok()).then_some(id)
}

async fn list_open(
    store: &Store,
    viewer: &Viewer,
    mut before: Option<u64>,
    limit: usize,
) -> Result<(Vec<Value>, Option<String>), ApiError> {
    let mut tickets = Vec::with_capacity(limit);
    let mut last_returned = None;
    let mut last_scanned = before;
    let mut scanned = 0;
    let mut more_visible = false;
    let mut more_entries = false;

    while scanned < MAX_SCAN {
        let batch = SCAN_BATCH.min(MAX_SCAN - scanned);
        let found = store
            .list_numeric_suffix_before(OPEN_PREFIX, before, batch)
            .await
            .map_err(storage_error)?;
        if found.is_empty() {
            break;
        }
        let full_batch = found.len() == batch;
        for (key, entry) in found {
            let channel = key
                .strip_prefix(OPEN_PREFIX)
                .and_then(|id| id.parse::<u64>().ok())
                .ok_or_else(|| ApiError::Internal("invalid open ticket key".into()))?;
            before = Some(channel);
            last_scanned = Some(channel);
            scanned += 1;
            let revision = entry.revision;
            let ticket = decode::<OpenTicket>(entry)?;
            if ticket.closed {
                continue;
            }
            let staff_roles = open_staff_roles(&ticket, &viewer.default_staff_roles);
            if can_view(viewer, &staff_roles) {
                if tickets.len() == limit {
                    more_visible = true;
                    break;
                }
                tickets.push(ticket_json(TicketView {
                    channel,
                    number: ticket.number,
                    opener: ticket.opener,
                    claimed_by: ticket.claimed_by,
                    created: ticket.created,
                    state: if ticket.closing.is_some() {
                        "closing"
                    } else {
                        "open"
                    },
                    closed_at: None,
                    closer_id: None,
                    reason: None,
                    revision: Some(revision),
                    panel_id: panel_id(ticket.panel.as_ref()),
                    labels: ticket.labels.clone(),
                    rating: None,
                    has_transcript: false,
                    transcript: None,
                }));
                last_returned = Some(channel);
            }
            if scanned == MAX_SCAN {
                break;
            }
        }
        if more_visible {
            break;
        }
        if scanned == MAX_SCAN {
            if let Some(cursor) = last_scanned {
                more_entries = !store
                    .list_numeric_suffix_before(OPEN_PREFIX, Some(cursor), 1)
                    .await
                    .map_err(storage_error)?
                    .is_empty();
            }
            break;
        }
        if !full_batch {
            break;
        }
    }

    let next_before = if more_visible {
        last_returned
    } else if more_entries {
        last_scanned
    } else {
        None
    }
    .map(|cursor| cursor.to_string());
    Ok((tickets, next_before))
}

async fn list_closed(
    store: &Store,
    viewer: &Viewer,
    before: Option<i64>,
    limit: usize,
    filters: &ClosedFilters,
    count_total: bool,
) -> Result<(Vec<Value>, Option<String>, Option<usize>, Option<bool>), ApiError> {
    let mut after = before.map(history_after_number).transpose()?;
    let mut tickets = Vec::with_capacity(limit);
    let mut last_returned = None;
    let mut last_scanned_number = None;
    let mut privacy_checks = Vec::with_capacity(limit);
    let mut scanned = 0;
    let mut page_has_more = false;
    let mut more_entries = false;

    while scanned < MAX_SCAN {
        let batch = SCAN_BATCH.min(MAX_SCAN - scanned);
        let found = if let Some(after) = after.as_deref() {
            store
                .list_after(HISTORY_PREFIX, after, batch)
                .await
                .map_err(storage_error)?
        } else {
            store
                .list(HISTORY_PREFIX, batch)
                .await
                .map_err(storage_error)?
        };
        if found.is_empty() {
            break;
        }
        let full_batch = found.len() == batch;
        for (key, entry) in found {
            let revision = entry.revision;
            let history = decode::<TicketHistory>(entry)?;
            if history.number <= 0 || history.number == i64::MAX {
                return Err(ApiError::Internal("invalid ticket history number".into()));
            }
            let expected_prefix = format!("{HISTORY_PREFIX}{:019}:", i64::MAX - history.number);
            let key_channel = key
                .rsplit(':')
                .next()
                .and_then(|id| id.parse::<u64>().ok())
                .ok_or_else(|| ApiError::Internal("invalid ticket history key".into()))?;
            if !key.starts_with(&expected_prefix) || key_channel != history.channel_id {
                return Err(ApiError::Internal(
                    "ticket history key does not match its record".into(),
                ));
            }
            after = Some(key.clone());
            last_scanned_number = Some(history.number);
            scanned += 1;
            if history.status == "closed"
                && can_view_history(viewer, &history)
                && filters.matches(&history)
            {
                privacy_checks.push((key.clone(), history.privacy_generation));
                if tickets.len() < limit {
                    tickets.push(ticket_json(TicketView {
                        channel: history.channel_id,
                        number: history.number,
                        opener: history.opener,
                        claimed_by: history.claimed_by,
                        created: history.created,
                        state: "closed",
                        closed_at: history.closed,
                        closer_id: history.closer,
                        reason: history.reason.as_deref(),
                        revision: Some(revision),
                        panel_id: panel_id(history.panel.as_ref()),
                        labels: history.labels.clone(),
                        rating: history.rating.as_ref().map(|rating| rating.score),
                        has_transcript: history.transcript_pages > 0,
                        transcript: None,
                    }));
                    last_returned = Some(history.number);
                } else {
                    page_has_more = true;
                    break;
                }
            }
            if scanned == MAX_SCAN {
                break;
            }
        }
        if page_has_more {
            break;
        }
        if scanned == MAX_SCAN {
            if let Some(after) = after.as_deref() {
                more_entries = !store
                    .list_after(HISTORY_PREFIX, after, 1)
                    .await
                    .map_err(storage_error)?
                    .is_empty();
            }
            break;
        }
        if !full_batch {
            break;
        }
    }

    for (key, generation) in privacy_checks {
        if !history_privacy_generation_current(store, &key, generation).await? {
            return Err(no_ticket());
        }
    }
    let next_before = if page_has_more {
        last_returned
    } else if more_entries {
        last_scanned_number
    } else {
        None
    }
    .map(|number| number.to_string());
    let (total, total_complete) = if count_total {
        let (total, complete) = count_closed(store, viewer, filters).await?;
        (Some(total), Some(complete))
    } else {
        (None, None)
    };
    Ok((tickets, next_before, total, total_complete))
}

async fn count_closed(
    store: &Store,
    viewer: &Viewer,
    filters: &ClosedFilters,
) -> Result<(usize, bool), ApiError> {
    let mut after: Option<String> = None;
    let mut scanned = 0;
    let mut total = 0;
    let mut complete = false;
    let mut privacy_checks = Vec::with_capacity(MAX_SCAN);

    while scanned < MAX_SCAN {
        let batch = SCAN_BATCH.min(MAX_SCAN - scanned);
        let found = if let Some(after) = after.as_deref() {
            store
                .list_after(HISTORY_PREFIX, after, batch)
                .await
                .map_err(storage_error)?
        } else {
            store
                .list(HISTORY_PREFIX, batch)
                .await
                .map_err(storage_error)?
        };
        if found.is_empty() {
            complete = true;
            break;
        }
        let full_batch = found.len() == batch;
        for (key, entry) in found {
            let history = decode::<TicketHistory>(entry)?;
            if history.number <= 0 || history.number == i64::MAX {
                return Err(ApiError::Internal("invalid ticket history number".into()));
            }
            let expected_prefix = format!("{HISTORY_PREFIX}{:019}:", i64::MAX - history.number);
            let key_channel = key
                .rsplit(':')
                .next()
                .and_then(|id| id.parse::<u64>().ok())
                .ok_or_else(|| ApiError::Internal("invalid ticket history key".into()))?;
            if !key.starts_with(&expected_prefix) || key_channel != history.channel_id {
                return Err(ApiError::Internal(
                    "ticket history key does not match its record".into(),
                ));
            }
            after = Some(key.clone());
            scanned += 1;
            if history.status == "closed"
                && can_view_history(viewer, &history)
                && filters.matches(&history)
            {
                total += 1;
                privacy_checks.push((key, history.privacy_generation));
            }
            if scanned == MAX_SCAN {
                break;
            }
        }
        if scanned == MAX_SCAN {
            if let Some(after) = after.as_deref() {
                complete = store
                    .list_after(HISTORY_PREFIX, after, 1)
                    .await
                    .map_err(storage_error)?
                    .is_empty();
            }
            break;
        }
        if !full_batch {
            complete = true;
            break;
        }
    }

    for (key, generation) in privacy_checks {
        if !history_privacy_generation_current(store, &key, generation).await? {
            return Err(no_ticket());
        }
    }
    Ok((total, complete))
}

async fn transcript(
    store: &Store,
    channel: u64,
    history: &TicketHistory,
    start: usize,
    limit: usize,
) -> Result<Value, ApiError> {
    if start > history.transcript_pages {
        return Err(ApiError::BadRequest(
            "start is past the end of the ticket transcript".into(),
        ));
    }
    let end = start
        .checked_add(limit)
        .ok_or_else(|| ApiError::BadRequest("start is too large".into()))?
        .min(history.transcript_pages);
    let mut pages = Vec::with_capacity(end - start);
    if history.transcript_pages > 0 {
        let attempt = history
            .transcript_attempt
            .ok_or_else(|| ApiError::Internal("ticket transcript has no generation".into()))?;
        for index in start..end {
            let key = format!("{HISTORY_CONTENT_PREFIX}{channel}:{attempt}:{index:04}");
            let Some(text) = store.get::<String>(&key).await.map_err(storage_error)? else {
                return Err(ApiError::NotFound(
                    "the ticket transcript is no longer available".into(),
                ));
            };
            let messages = if index < history.transcript_span_pages {
                let span_key = format!("{HISTORY_SPAN_PREFIX}{channel}:{attempt}:{index:04}");
                match store
                    .get::<Vec<TranscriptSpan>>(&span_key)
                    .await
                    .map_err(storage_error)?
                {
                    Some(spans) => transcript_messages(&text, spans)?,
                    None => Vec::new(),
                }
            } else {
                Vec::new()
            };
            pages.push(json!({
                "index": index,
                "text": text,
                "messages": messages,
            }));
        }
    }
    Ok(json!({
        "pages": pages,
        "next_page": (end < history.transcript_pages).then_some(end),
        "truncated": history.transcript_truncated,
    }))
}

fn transcript_messages(
    page_text: &str,
    spans: Vec<TranscriptSpan>,
) -> Result<Vec<Value>, ApiError> {
    let page_chars = page_text.chars().count();
    let mut messages = Vec::new();
    for span in spans {
        let TranscriptSpanPart::Message {
            message_id,
            author_id,
        } = span.part
        else {
            continue;
        };
        let start =
            usize::try_from(span.start).map_err(|error| ApiError::Internal(error.to_string()))?;
        let end =
            usize::try_from(span.end).map_err(|error| ApiError::Internal(error.to_string()))?;
        if start >= end || end > page_chars || message_id == 0 || author_id == 0 {
            return Err(ApiError::Internal(
                "ticket transcript message span is invalid".into(),
            ));
        }
        let rich = span.rich.as_ref().filter(|value| value.is_object());
        let author_name = rich
            .and_then(|value| value.get("author_name"))
            .and_then(Value::as_str)
            .filter(|value| value.chars().count() <= 256 && !value.chars().any(char::is_control));
        let created_at = rich
            .and_then(|value| value.get("created_at"))
            .and_then(Value::as_str)
            .filter(|value| value.len() <= 64 && !value.chars().any(char::is_control));
        let array = |key: &str| {
            rich.and_then(|value| value.get(key))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        messages.push(json!({
            "start": span.start,
            "end": span.end,
            "message_id": message_id.to_string(),
            "author_id": author_id.to_string(),
            "author_name": author_name,
            "created_at": created_at,
            "attachments": array("attachments"),
            "embeds": array("embeds"),
            "components": array("components"),
            "mentions": array("mentions"),
        }));
    }
    Ok(messages)
}

fn history_after_number(number: i64) -> Result<String, ApiError> {
    if number <= 0 || number == i64::MAX {
        return Err(ApiError::BadRequest(
            "before must be a positive ticket number".into(),
        ));
    }
    let reverse = i64::MAX - number;
    Ok(format!("{HISTORY_PREFIX}{reverse:019}:{CURSOR_SENTINEL}"))
}

fn parse_ticket_number(raw: &str) -> Result<i64, ApiError> {
    let id = parse_snowflake(raw)?;
    i64::try_from(id)
        .map_err(|_| ApiError::BadRequest("before must be a positive ticket number".into()))
}

fn parse_snowflake(raw: &str) -> Result<u64, ApiError> {
    let id = raw
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && i64::try_from(*id).is_ok())
        .ok_or_else(|| ApiError::BadRequest("before must be a positive decimal ID".into()))?;
    Ok(id)
}

fn checked_snowflake(id: u64, name: &str) -> Result<u64, ApiError> {
    if id == 0 || i64::try_from(id).is_err() {
        return Err(ApiError::BadRequest(format!(
            "the {name} ID must be a positive snowflake"
        )));
    }
    Ok(id)
}

fn decode<T: serde::de::DeserializeOwned>(entry: Entry) -> Result<T, ApiError> {
    serde_json::from_value(entry.value)
        .map_err(|error| ApiError::Internal(format!("invalid ticket record: {error}")))
}

fn panel_id(panel: Option<&Value>) -> Option<&str> {
    panel.and_then(|panel| {
        panel
            .as_str()
            .or_else(|| panel.get("id").and_then(Value::as_str))
    })
}

struct TicketView<'a> {
    channel: u64,
    revision: Option<i64>,
    number: i64,
    opener: u64,
    claimed_by: Option<u64>,
    created: i64,
    state: &'a str,
    closed_at: Option<i64>,
    closer_id: Option<u64>,
    reason: Option<&'a str>,
    panel_id: Option<&'a str>,
    labels: Vec<String>,
    rating: Option<u8>,
    has_transcript: bool,
    transcript: Option<Value>,
}

fn ticket_json(view: TicketView<'_>) -> Value {
    let mut ticket = json!({
        "channel_id": view.channel.to_string(),
        "number": view.number,
        "opener": view.opener.to_string(),
        "claimed_by": view.claimed_by.map(|id| id.to_string()),
        "created": view.created,
        "state": view.state,
        "closed": view.closed_at,
        "closer": view.closer_id.map(|id| id.to_string()),
        "reason": view.reason,
        "panel": view.panel_id,
        "labels": view.labels,
        "rating": view.rating,
        "has_transcript": view.has_transcript,
    });
    if let Some(revision) = view.revision {
        ticket["revision"] = json!(revision);
    }
    if let Some(transcript) = view.transcript {
        ticket["transcript"] = transcript;
    }
    ticket
}

fn storage_error(error: StorageError) -> ApiError {
    match error {
        StorageError::Unavailable | StorageError::Backend(_) => ApiError::Unavailable,
        other => ApiError::Internal(other.to_string()),
    }
}

fn no_ticket() -> ApiError {
    ApiError::NotFound("no such ticket".into())
}
