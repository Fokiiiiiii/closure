//! The cases of a server for the dashboard: a list with filters, one case, a summary, a CSV, a note,
//! and the change of a reason or of whether a case is hidden.
//!
//! A call with `X-Closure-Actor` is the call of a person: they must be a moderator of the server
//! to read, and allowed to edit the dashboard to change anything. A call without it is the bot's
//! own token, which is trusted.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use closure_core::cases::{
    Case, CaseBackend, CaseEdit, CaseError, CaseFilter, CaseKind, CaseSource, CaseState, NewCase,
};
use closure_core::event::{GuildId, UserId};
use closure_core::module::ModuleId;
use closure_core::permission::Level;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    ApiState,
    access::evaluate,
    config::{actor, guild_id, stored},
    error::ApiError,
};

/// The most cases in one page of the list.
const MAX_PAGE: usize = 100;
/// The page when the caller does not say.
const DEFAULT_PAGE: usize = 25;
/// The most cases in one CSV.
const MAX_EXPORT: usize = 5000;
/// How many are read at a time for the CSV.
const EXPORT_PAGE: usize = 500;
/// The longest window of the summary.
const MAX_DAYS: i64 = 366;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// What the caller may do with the cases of a server.
struct Who {
    guild: GuildId,
    /// The person, when the call is a person's.
    person: Option<UserId>,
    /// May change a case (add a note, edit a reason, hide).
    may_edit: bool,
}

async fn who(state: &ApiState, raw: u64, headers: &HeaderMap) -> Result<Who, ApiError> {
    let guild = guild_id(raw)?;
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }
    let Some(person) = actor(headers)? else {
        return Ok(Who {
            guild,
            person: None,
            may_edit: true,
        });
    };
    let (_, current) = stored(state, guild).await?;
    let settings = state.inner.settings.compiler().compile(&current)?;
    let access = evaluate(state, guild, person, settings.permissions()).await?;
    if access.level < Level::MODERATOR.get() {
        return Err(ApiError::Forbidden(
            "the cases are for the moderators of the server".into(),
        ));
    }
    Ok(Who {
        guild,
        person: Some(person),
        may_edit: access.allowed,
    })
}

fn may_edit(who: &Who) -> Result<(), ApiError> {
    if who.may_edit {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "changing the cases needs the permission to edit the dashboard".into(),
        ))
    }
}

fn state_of(case: &Case, at: i64) -> &'static str {
    if case.revoked.is_some() {
        CaseState::Revoked.as_str()
    } else if case.expires_at.is_some_and(|end| end <= at) {
        CaseState::Expired.as_str()
    } else {
        CaseState::Active.as_str()
    }
}

/// A case as the API shows it: ids are texts, so that none is rounded by a client.
fn case_json(case: &Case, at: i64) -> Value {
    json!({
        "number": case.number,
        "module": case.module.as_str(),
        "kind": case.kind.as_str(),
        "target": case.target.get().to_string(),
        "moderator": case.moderator.get().to_string(),
        "reason": case.reason,
        "created_at": case.created_at,
        "expires_at": case.expires_at,
        "state": state_of(case, at),
        "revoked": case.revoked.as_ref().map(|revoked| json!({
            "by": revoked.by.get().to_string(),
            "at": revoked.at,
            "reason": revoked.reason,
        })),
        "source": case.source.as_str(),
        "hidden": case.hidden,
    })
}

fn case_error(error: CaseError) -> ApiError {
    match error {
        CaseError::NotFound => ApiError::NotFound("no such case".into()),
        CaseError::InvalidKind(why) | CaseError::InvalidReason(why) => ApiError::BadRequest(why),
        CaseError::QuotaExceeded { limit } => {
            ApiError::Conflict(format!("the server already has {limit} cases"))
        }
        CaseError::AlreadyRevoked => ApiError::Conflict("the case is already revoked".into()),
        other => ApiError::Internal(other.to_string()),
    }
}

/// What a list, and the CSV of it, ask for.
#[derive(Deserialize, Default)]
pub(crate) struct Find {
    user: Option<String>,
    moderator: Option<String>,
    kind: Option<String>,
    source: Option<String>,
    state: Option<String>,
    since: Option<i64>,
    until: Option<i64>,
    text: Option<String>,
    hidden: Option<bool>,
    before: Option<i64>,
    limit: Option<usize>,
}

fn user_id(text: &str, what: &str) -> Result<UserId, ApiError> {
    text.parse::<u64>()
        .ok()
        .filter(|id| *id > 0 && i64::try_from(*id).is_ok())
        .map(UserId::new)
        .ok_or_else(|| ApiError::BadRequest(format!("{what} must be an id")))
}

fn filter_of(find: &Find, who: &Who) -> Result<CaseFilter, ApiError> {
    let include_hidden = find.hidden.unwrap_or(false);
    if include_hidden {
        may_edit(who)?;
    }
    Ok(CaseFilter {
        target: find
            .user
            .as_deref()
            .map(|id| user_id(id, "user"))
            .transpose()?,
        moderator: find
            .moderator
            .as_deref()
            .map(|id| user_id(id, "moderator"))
            .transpose()?,
        kind: find
            .kind
            .as_deref()
            .map(|text| {
                CaseKind::new(&text.trim().to_lowercase())
                    .map(|kind| kind.as_str().to_owned())
                    .map_err(case_error)
            })
            .transpose()?,
        source: find
            .source
            .as_deref()
            .map(|text| {
                CaseSource::named(&text.trim().to_lowercase())
                    .ok_or_else(|| ApiError::BadRequest("unknown source".into()))
            })
            .transpose()?,
        state: find
            .state
            .as_deref()
            .map(|text| {
                CaseState::named(&text.trim().to_lowercase()).ok_or_else(|| {
                    ApiError::BadRequest("state must be active, expired or revoked".into())
                })
            })
            .transpose()?,
        since: find.since,
        until: find.until,
        text: find.text.clone().filter(|text| !text.trim().is_empty()),
        include_hidden,
    })
}

pub(crate) async fn list(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
    headers: HeaderMap,
    Query(find): Query<Find>,
) -> Result<Json<Value>, ApiError> {
    let who = who(&state, raw, &headers).await?;
    let filter = filter_of(&find, &who)?;
    let limit = find.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE);
    let at = now();
    // One more than a page tells whether there is another.
    let mut found = state
        .inner
        .db
        .search(who.guild, &filter, at, find.before, limit + 1)
        .await
        .map_err(case_error)?;
    let more = found.len() > limit;
    found.truncate(limit);
    let next_before = more.then(|| found.last().map(|case| case.number)).flatten();
    Ok(Json(json!({
        "cases": found.iter().map(|case| case_json(case, at)).collect::<Vec<_>>(),
        "next_before": next_before,
    })))
}

pub(crate) async fn get(
    State(state): State<ApiState>,
    Path((raw, number)): Path<(u64, i64)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let who = who(&state, raw, &headers).await?;
    let case = state
        .inner
        .db
        .get(who.guild, number)
        .await
        .map_err(case_error)?
        .ok_or_else(|| ApiError::NotFound("no such case".into()))?;
    Ok(Json(case_json(&case, now())))
}

#[derive(Deserialize)]
pub(crate) struct SummaryQuery {
    days: Option<i64>,
}

pub(crate) async fn summary(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
    headers: HeaderMap,
    Query(query): Query<SummaryQuery>,
) -> Result<Json<Value>, ApiError> {
    let who = who(&state, raw, &headers).await?;
    let days = query.days.unwrap_or(30);
    if !(1..=MAX_DAYS).contains(&days) {
        return Err(ApiError::BadRequest(format!(
            "days must be an integer from 1 to {MAX_DAYS}"
        )));
    }
    let mut result = state
        .inner
        .db
        .case_summary(who.guild, now() - days * 86_400)
        .await?;
    result["days"] = json!(days);
    Ok(Json(result))
}

/// A cell of the CSV. A text a spreadsheet would read as a formula starts with a quote instead.
fn cell(text: &str) -> String {
    let text = if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("\'{text}")
    } else {
        text.to_owned()
    };
    format!("\"{}\"", text.replace('"', "\"\""))
}

pub(crate) async fn export(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
    headers: HeaderMap,
    Query(find): Query<Find>,
) -> Result<Response, ApiError> {
    let who = who(&state, raw, &headers).await?;
    may_edit(&who)?;
    let filter = filter_of(&find, &who)?;
    let at = now();
    let mut csv = String::from(
        "number,created_at,kind,target,moderator,reason,state,expires_at,source,hidden\n",
    );
    let mut before = find.before;
    let mut written = 0;
    while written < MAX_EXPORT {
        let page = state
            .inner
            .db
            .search(
                who.guild,
                &filter,
                at,
                before,
                EXPORT_PAGE.min(MAX_EXPORT - written),
            )
            .await
            .map_err(case_error)?;
        let Some(last) = page.last() else { break };
        before = Some(last.number);
        written += page.len();
        for case in &page {
            let _ = writeln!(
                csv,
                "{},{},{},{},{},{},{},{},{},{}",
                case.number,
                case.created_at,
                cell(case.kind.as_str()),
                case.target.get(),
                case.moderator.get(),
                cell(case.reason.as_deref().unwrap_or("")),
                state_of(case, at),
                case.expires_at
                    .map_or_else(String::new, |end| end.to_string()),
                case.source.as_str(),
                case.hidden,
            );
        }
    }
    let mut response = csv.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"cases-{}.csv\"", who.guild))
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewNote {
    target: String,
    reason: String,
}

/// Records a note against a member, in the name of the person who asks.
pub(crate) async fn add_note(
    State(state): State<ApiState>,
    Path(raw): Path<u64>,
    headers: HeaderMap,
    Json(body): Json<NewNote>,
) -> Result<Json<Value>, ApiError> {
    let who = who(&state, raw, &headers).await?;
    may_edit(&who)?;
    let moderator = who.person.ok_or_else(|| {
        ApiError::BadRequest(
            "a note is made in the name of a person: X-Closure-Actor is needed".into(),
        )
    })?;
    if body.reason.trim().is_empty() {
        return Err(ApiError::BadRequest("a note needs a text".into()));
    }
    let module = ModuleId::new("dashboard").map_err(|e| ApiError::Internal(e.to_string()))?;
    let case = state
        .inner
        .db
        .create(
            &module,
            who.guild,
            NewCase {
                source: CaseSource::Dashboard,
                kind: CaseKind::new("note").map_err(case_error)?,
                target: user_id(&body.target, "target")?,
                moderator,
                reason: Some(body.reason),
                expires_at: None,
            },
        )
        .await
        .map_err(case_error)?;
    Ok(Json(case_json(&case, now())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Change {
    reason: Option<String>,
    hidden: Option<bool>,
}

/// Changes the reason of a case, or hides or shows it.
pub(crate) async fn change(
    State(state): State<ApiState>,
    Path((raw, number)): Path<(u64, i64)>,
    headers: HeaderMap,
    Json(body): Json<Change>,
) -> Result<Json<Value>, ApiError> {
    let who = who(&state, raw, &headers).await?;
    may_edit(&who)?;
    if body.reason.is_none() && body.hidden.is_none() {
        return Err(ApiError::BadRequest("say a reason or hidden".into()));
    }
    if body
        .reason
        .as_deref()
        .is_some_and(|text| text.trim().is_empty())
    {
        return Err(ApiError::BadRequest("a reason cannot be empty".into()));
    }
    let case = state
        .inner
        .db
        .edit(
            who.guild,
            number,
            CaseEdit {
                reason: body.reason,
                hidden: body.hidden,
                moderator: None,
            },
        )
        .await
        .map_err(case_error)?;
    Ok(Json(case_json(&case, now())))
}
