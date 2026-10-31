use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use closure_config::{
    GuildOverrides, View, basic_view, effective_to_yaml, merge_basic, overrides_to_yaml,
    parse_overrides,
};
use closure_core::event::GuildId;
use closure_db::AuditEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::id::Id;

use crate::ApiState;
use crate::error::ApiError;

const YAML: &str = "application/yaml";

/// Discord snowflakes are positive and fit in 63 bits.
fn guild_id(raw: u64) -> Result<GuildId, ApiError> {
    if raw == 0 || i64::try_from(raw).is_err() {
        return Err(ApiError::BadRequest(
            "the guild id must be a positive snowflake".into(),
        ));
    }
    Ok(Id::new(raw))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Json,
    Yaml,
}

/// YAML only when the client asks for it; JSON is the default.
fn negotiate(headers: &HeaderMap) -> Format {
    let wants_yaml = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("yaml"));
    if wants_yaml {
        Format::Yaml
    } else {
        Format::Json
    }
}

fn etag(revision: i64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{revision}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("\"0\""))
}

fn with_etag(mut response: Response, revision: i64) -> Response {
    response.headers_mut().insert(header::ETAG, etag(revision));
    response
}

fn yaml_response(text: String) -> Response {
    let mut response = text.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(YAML));
    response
}

/// `If-Match: "n"` names the revision being edited; `"0"` means "there is none yet".
fn expected_revision(headers: &HeaderMap) -> Result<Option<i64>, ApiError> {
    let value = headers
        .get(header::IF_MATCH)
        .ok_or(ApiError::PreconditionRequired)?
        .to_str()
        .map_err(|_| ApiError::BadRequest("If-Match must be a quoted revision".into()))?;
    let revision: i64 = value
        .trim()
        .trim_matches('"')
        .parse()
        .ok()
        .filter(|revision| *revision >= 0)
        .ok_or_else(|| ApiError::BadRequest("If-Match must be a quoted revision".into()))?;
    Ok((revision > 0).then_some(revision))
}

/// The `view` query parameter: which settings a read shows or a write takes.
#[derive(Debug, Deserialize)]
pub(crate) struct ViewQuery {
    view: Option<String>,
    locale: Option<String>,
}

/// Advanced unless asked otherwise, so a request that names no view behaves as it always did.
pub(crate) fn view_of(query: &ViewQuery) -> Result<View, ApiError> {
    match query.view.as_deref() {
        None => Ok(View::Advanced),
        Some(name) => View::parse(name)
            .ok_or_else(|| ApiError::BadRequest("view must be `basic` or `advanced`".into())),
    }
}

/// The language of the descriptions a read shows; English unless asked for another one that exists.
pub(crate) fn locale_of(query: &ViewQuery) -> Result<closure_core::locale::Locale, ApiError> {
    match query.locale.as_deref() {
        None => Ok(closure_core::locale::Locale::DEFAULT),
        Some(tag) => tag.parse().map_err(|_| {
            ApiError::BadRequest("locale must be a language tag the bot speaks".into())
        }),
    }
}

/// Documents may be sent as YAML or JSON (JSON is YAML), whatever the content type says.
fn body_text(body: &Bytes) -> Result<&str, ApiError> {
    std::str::from_utf8(body)
        .map_err(|_| ApiError::BadRequest("the body must be UTF-8 text".into()))
}

fn parse_body(body: &Bytes) -> Result<GuildOverrides, ApiError> {
    Ok(parse_overrides(body_text(body)?)?)
}

/// The stored document and its revision; a guild without one is the empty document at revision 0.
async fn stored(state: &ApiState, guild: GuildId) -> Result<(i64, GuildOverrides), ApiError> {
    Ok(match state.inner.db.load_config(guild).await? {
        Some(stored) => (stored.revision, stored.overrides),
        None => (0, GuildOverrides::default()),
    })
}

pub(crate) async fn get_config(
    State(state): State<ApiState>,
    Path(guild): Path<u64>,
    Query(query): Query<ViewQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let guild = guild_id(guild)?;
    let view = view_of(&query)?;
    let (revision, overrides) = stored(&state, guild).await?;
    let overrides = match view {
        View::Advanced => overrides,
        View::Basic => basic_view(&overrides, state.inner.settings.compiler().registry())?,
    };
    let response = match negotiate(&headers) {
        Format::Yaml => yaml_response(overrides_to_yaml(&overrides)?),
        Format::Json => Json(&overrides).into_response(),
    };
    Ok(with_etag(response, revision))
}

pub(crate) async fn put_config(
    State(state): State<ApiState>,
    Path(guild): Path<u64>,
    Query(query): Query<ViewQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let guild = guild_id(guild)?;
    let view = view_of(&query)?;
    let expected = expected_revision(&headers)?;
    let overrides = match view {
        View::Advanced => parse_body(&body)?,
        View::Basic => {
            // The patch applies to the revision the client edited, or it is a stale edit.
            let (revision, current) = stored(&state, guild).await?;
            if revision != expected.unwrap_or(0) {
                return Err(ApiError::PreconditionFailed { current: revision });
            }
            merge_basic(
                &current,
                body_text(&body)?,
                state.inner.settings.compiler().registry(),
            )?
        }
    };
    save(&state, guild, expected, overrides).await
}

/// The write both views share: validate, check the server, store, install and audit.
async fn save(
    state: &ApiState,
    guild: GuildId,
    expected: Option<i64>,
    overrides: GuildOverrides,
) -> Result<Response, ApiError> {
    // Validate before touching the database: an invalid document must never be stored.
    let settings = state.inner.settings.compiler().compile(&overrides)?;
    // Only servers the bot is in can be configured; this keeps stray ids out of the database.
    if !state.inner.db.is_guild_active(guild).await? {
        return Err(ApiError::NotFound("the bot is not in that server".into()));
    }

    let revision = state
        .inner
        .db
        .save_config(guild, expected, &overrides, None)
        .await?;
    if !state.inner.settings.install(guild, revision, settings) {
        // A concurrent edit with a higher revision is already live; that is the one to keep.
        tracing::debug!(%guild, revision, "a newer configuration was already installed");
    }

    let audit = AuditEvent {
        guild_id: guild,
        actor: None,
        action: "config.update".into(),
        payload: json!({ "revision": revision }),
    };
    if let Err(error) = state.inner.db.record_audit(&audit).await {
        // The change itself is committed; failing the request now would only invite a retry.
        tracing::warn!(%error, %guild, "could not record the configuration change in the audit trail");
    }
    Ok(with_etag(
        Json(json!({ "revision": revision })).into_response(),
        revision,
    ))
}

/// Checks a document without storing it, and shows what it would resolve to.
pub(crate) async fn validate(
    State(state): State<ApiState>,
    Path(guild): Path<u64>,
    Query(query): Query<ViewQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let guild = guild_id(guild)?;
    let overrides = match view_of(&query)? {
        View::Advanced => parse_body(&body)?,
        View::Basic => {
            let (_, current) = stored(&state, guild).await?;
            merge_basic(
                &current,
                body_text(&body)?,
                state.inner.settings.compiler().registry(),
            )?
        }
    };
    let settings = state.inner.settings.compiler().compile(&overrides)?;
    Ok(match negotiate(&headers) {
        Format::Yaml => yaml_response(effective_to_yaml(&settings.effective)?),
        Format::Json => {
            Json(json!({ "valid": true, "effective": settings.effective })).into_response()
        }
    })
}

/// The complete configuration with every default filled in.
pub(crate) async fn effective(
    State(state): State<ApiState>,
    Path(guild): Path<u64>,
    Query(query): Query<ViewQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let guild = guild_id(guild)?;
    // Checked, then ignored: the complete configuration is the same in both views.
    view_of(&query)?;
    let (revision, overrides) = stored(&state, guild).await?;
    let settings = state.inner.settings.compiler().compile(&overrides)?;
    let response = match negotiate(&headers) {
        Format::Yaml => yaml_response(effective_to_yaml(&settings.effective)?),
        Format::Json => Json(&settings.effective).into_response(),
    };
    Ok(with_etag(response, revision))
}

#[derive(Debug, Deserialize)]
pub(crate) struct HistoryQuery {
    limit: Option<u32>,
}

#[derive(Serialize)]
struct HistoryEntry {
    revision: i64,
    created_at_unix: i64,
    actor: Option<u64>,
    document: GuildOverrides,
}

pub(crate) async fn history(
    State(state): State<ApiState>,
    Path(guild): Path<u64>,
    Query(query): Query<HistoryQuery>,
) -> Result<Response, ApiError> {
    let guild = guild_id(guild)?;
    let entries = state
        .inner
        .db
        .config_history(guild, query.limit.unwrap_or(20))
        .await?;
    let body: Vec<HistoryEntry> = entries
        .into_iter()
        .map(|entry| HistoryEntry {
            revision: entry.revision,
            created_at_unix: entry.created_at.unix_timestamp(),
            actor: entry.actor.map(Id::get),
            document: entry.overrides,
        })
        .collect();
    Ok((StatusCode::OK, Json(body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_MATCH, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn if_match_names_the_edited_revision_and_zero_means_create() {
        assert_eq!(expected_revision(&headers("\"3\"")).unwrap(), Some(3));
        assert_eq!(expected_revision(&headers("3")).unwrap(), Some(3));
        assert_eq!(expected_revision(&headers("\"0\"")).unwrap(), None);
    }

    #[test]
    fn a_missing_or_malformed_if_match_is_rejected() {
        assert!(matches!(
            expected_revision(&HeaderMap::new()),
            Err(ApiError::PreconditionRequired)
        ));
        for bad in ["*", "\"x\"", "\"-1\"", ""] {
            assert!(
                matches!(
                    expected_revision(&headers(bad)),
                    Err(ApiError::BadRequest(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn guild_ids_must_be_positive_snowflakes() {
        assert!(guild_id(1).is_ok());
        assert!(guild_id(0).is_err());
        assert!(guild_id(u64::MAX).is_err());
    }

    #[test]
    fn json_is_the_default_representation() {
        let mut headers = HeaderMap::new();
        assert_eq!(negotiate(&headers), Format::Json);
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/yaml"));
        assert_eq!(negotiate(&headers), Format::Yaml);
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        assert_eq!(negotiate(&headers), Format::Json);
    }
}
