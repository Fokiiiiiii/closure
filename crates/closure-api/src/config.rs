use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use closure_config::{
    ConfigCompiler, GuildOverrides, GuildSettings, View, basic_view, effective_to_yaml,
    merge_basic, overrides_to_yaml, parse_overrides,
};
use closure_core::event::{GuildId, UserId};
use closure_core::integration_secrets::{IntegrationSecretGeneration, new_integration_generation};
use closure_core::module::ModuleId;
use closure_db::AuditEvent;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use twilight_model::id::Id;

use crate::ApiState;
use crate::error::ApiError;

const YAML: &str = "application/yaml";
const PRIVILEGED_FIELDS: [&str; 2] = ["permissions", "moderator_roles"];

/// Discord snowflakes are positive and fit in 63 bits.
pub(crate) fn guild_id(raw: u64) -> Result<GuildId, ApiError> {
    if raw == 0 || i64::try_from(raw).is_err() {
        return Err(ApiError::BadRequest(
            "the guild id must be a positive snowflake".into(),
        ));
    }
    Ok(Id::new(raw))
}

pub(crate) fn actor(headers: &HeaderMap) -> Result<Option<UserId>, ApiError> {
    let mut values = headers.get_all("x-closure-actor").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    let raw = value
        .to_str()
        .ok()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|id| i64::try_from(*id).is_ok())
        .and_then(Id::new_checked);
    if values.next().is_some() || raw.is_none() {
        return Err(ApiError::BadRequest(
            "X-Closure-Actor must be a nonzero decimal user id".into(),
        ));
    }
    Ok(raw)
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

pub(super) fn with_etag(mut response: Response, revision: i64) -> Response {
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
pub(super) fn expected_revision(headers: &HeaderMap) -> Result<Option<i64>, ApiError> {
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
pub(crate) fn locale_of(query: &ViewQuery) -> closure_core::locale::Locale {
    query
        .locale
        .as_deref()
        .and_then(closure_core::locale::Locale::from_discord_tag)
        .unwrap_or(closure_core::locale::Locale::DEFAULT)
}

#[cfg(test)]
mod locale_tests {
    use super::{ViewQuery, locale_of};
    use closure_core::locale::Locale;

    #[test]
    fn schema_locale_defaults_for_unknown_tags() {
        for (tag, expected) in [
            ("ja", Locale::Ja),
            ("en", Locale::En),
            ("fr", Locale::DEFAULT),
        ] {
            let query = ViewQuery {
                view: None,
                locale: Some(tag.into()),
            };
            assert_eq!(locale_of(&query), expected);
        }
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
pub(crate) async fn stored(
    state: &ApiState,
    guild: GuildId,
) -> Result<(i64, GuildOverrides), ApiError> {
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
    let actor = actor(&headers)?;
    let expected = expected_revision(&headers)?;
    let (revision, current) = stored(&state, guild).await?;
    if revision != expected.unwrap_or(0) {
        return Err(ApiError::PreconditionFailed { current: revision });
    }
    let compiler = state.inner.settings.compiler();
    let overrides = match view {
        View::Advanced => parse_body(&body)?,
        View::Basic => merge_basic(&current, body_text(&body)?, compiler.registry())?,
    };
    if let Some(user) = actor {
        let access = crate::access::actor_access(&state, guild, user, &current).await?;
        // The guard of the dashboard asked this before it let the request through; a permission
        // taken away since then, or the writer who is no longer a member, stops it here too.
        if !access.allowed {
            return Err(ApiError::Forbidden(
                "the member may not edit the dashboard".into(),
            ));
        }
        let level = access.level;
        if level < 100 {
            if privileged_changed(&current, &overrides) {
                return Err(ApiError::Forbidden(
                    "changing permissions or moderator_roles requires owner level".into(),
                ));
            }
            let settings = compiler.compile(&current)?;
            if stronger_role_added(&current, &overrides, level, settings.permissions())? {
                return Err(ApiError::Forbidden(
                    "adding a stronger role reference requires owner level".into(),
                ));
            }
        }
    }
    let (overrides, active_generations, retirements) =
        normalize_integration_generations(guild, compiler, &current, &overrides)?;
    let rerouted = credential_routes_changed(&current, &overrides)?;
    // A dropped secret name can carry a stored value too, so the permission is checked whenever a member saves.
    let can_retire_secrets = if actor.is_none() {
        false
    } else {
        let (_, viewer) = crate::tickets::viewer(&state, guild.get(), &headers).await?;
        viewer.actor.owner || viewer.actor.administrator
    };
    if rerouted && actor.is_some() && !can_retire_secrets {
        return Err(ApiError::Forbidden(
            "changing where an integration's credentials are sent requires owner or administrator"
                .into(),
        ));
    }
    save(
        &state,
        guild,
        expected,
        overrides,
        actor,
        &active_generations,
        &retirements,
        can_retire_secrets,
    )
    .await
}

/// The integration settings a stored secret travels through: the URL, the authentication kind and
/// the headers that carry a secret. Integrations that carry no secret have no route.
fn credential_routes(
    document: &GuildOverrides,
) -> Result<BTreeMap<String, serde_json::Value>, ApiError> {
    let value =
        serde_json::to_value(document).map_err(|error| ApiError::Internal(error.to_string()))?;
    let integrations = value
        .pointer("/modules/tickets/config/integrations")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut routes = BTreeMap::new();
    for integration in integrations {
        let Some(id) = integration.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let auth = integration
            .get("auth")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("none");
        let secrets: Vec<&str> = integration
            .get("secrets")
            .and_then(serde_json::Value::as_array)
            .map(|names| names.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        let secret_headers: Vec<&serde_json::Value> = integration
            .get("headers")
            .and_then(serde_json::Value::as_array)
            .map(|headers| {
                headers
                    .iter()
                    .filter(|header| {
                        header
                            .get("value_template")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|template| {
                                template.contains("%token%")
                                    || secrets
                                        .iter()
                                        .any(|name| template.contains(&format!("%{name}%")))
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if auth == "none" && secret_headers.is_empty() {
            continue;
        }
        routes.insert(
            id.to_owned(),
            json!({
                "url": integration.get("url"),
                "auth": auth,
                "secret_headers": secret_headers,
            }),
        );
    }
    Ok(routes)
}

/// True when an edit changes where a stored secret would be sent, which needs owner or administrator.
fn credential_routes_changed(
    current: &GuildOverrides,
    next: &GuildOverrides,
) -> Result<bool, ApiError> {
    let before = credential_routes(current)?;
    let after = credential_routes(next)?;
    Ok(before
        .keys()
        .chain(after.keys())
        .any(|id| before.get(id) != after.get(id)))
}

/// Adds host-generated immutable generations to ticket integrations and returns active/retired scopes.
fn normalize_integration_generations(
    guild: GuildId,
    compiler: &ConfigCompiler,
    current: &GuildOverrides,
    overrides: &GuildOverrides,
) -> Result<
    (
        GuildOverrides,
        Vec<IntegrationSecretGeneration>,
        Vec<IntegrationSecretGeneration>,
    ),
    ApiError,
> {
    let current_settings = compiler.compile(current)?;
    let current_generations = integration_generations(guild, &current_settings)?;
    let mut serialized =
        serde_json::to_value(overrides).map_err(|error| ApiError::Internal(error.to_string()))?;

    if let Some(integrations_value) = serialized
        .get_mut("modules")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|modules| modules.get_mut("tickets"))
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|module| module.get_mut("config"))
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|config| config.get_mut("integrations"))
    {
        let integrations = integrations_value
            .as_array_mut()
            .ok_or_else(|| ApiError::BadRequest("ticket integrations must be an array".into()))?;
        let mut seen = BTreeSet::new();
        for integration in integrations {
            let object = integration.as_object_mut().ok_or_else(|| {
                ApiError::BadRequest("each ticket integration must be an object".into())
            })?;
            let id = object
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| valid_integration_id(id))
                .ok_or_else(|| ApiError::BadRequest("ticket integration ID is invalid".into()))?
                .to_owned();
            if !seen.insert(id.clone()) {
                return Err(ApiError::BadRequest(
                    "ticket integration IDs must be unique".into(),
                ));
            }
            let supplied = match object.get("generation") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(value)) if value.is_empty() => None,
                Some(serde_json::Value::String(value)) => Some(value.as_str()),
                Some(_) => {
                    return Err(ApiError::BadRequest(
                        "ticket integration generation is managed by the host".into(),
                    ));
                }
            };
            if let Some(existing) = current_generations.get(&id) {
                let expected = existing.generation();
                if supplied.is_some_and(|generation| generation != expected) {
                    return Err(ApiError::BadRequest(
                        "ticket integration generation cannot be changed".into(),
                    ));
                }
                object.insert(
                    "generation".into(),
                    serde_json::Value::String(expected.to_owned()),
                );
            } else {
                if supplied.is_some() {
                    return Err(ApiError::BadRequest(
                        "ticket integration generation cannot be supplied".into(),
                    ));
                }
                let generation = new_integration_generation().map_err(|_| ApiError::Unavailable)?;
                object.insert("generation".into(), serde_json::Value::String(generation));
            }
        }
    }

    let overrides: GuildOverrides = serde_json::from_value(serialized)
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let next_settings = compiler.compile(&overrides)?;
    let active_generations = integration_generations(guild, &next_settings)?;
    let retirements = current_generations
        .iter()
        .filter(|(id, old)| {
            active_generations
                .get(*id)
                .is_none_or(|new| new.generation() != old.generation())
        })
        .map(|(_, generation)| generation.clone())
        .collect::<Vec<_>>();
    let active_generations = active_generations.into_values().collect::<Vec<_>>();
    Ok((overrides, active_generations, retirements))
}

fn integration_generations(
    guild: GuildId,
    settings: &GuildSettings,
) -> Result<BTreeMap<String, IntegrationSecretGeneration>, ApiError> {
    let module = ModuleId::new("tickets").map_err(|error| ApiError::Internal(error.to_string()))?;
    let integrations = settings
        .module(&module)
        .and_then(|module| module.config.get("integrations"))
        .and_then(serde_json::Value::as_array);
    let Some(integrations) = integrations else {
        return Ok(BTreeMap::new());
    };
    let mut generations = BTreeMap::new();
    for integration in integrations {
        let Some(id) = integration.get("id").and_then(serde_json::Value::as_str) else {
            return Err(ApiError::Internal(
                "the stored ticket integration has no ID".into(),
            ));
        };
        let Some(generation) = integration
            .get("generation")
            .and_then(serde_json::Value::as_str)
            .filter(|generation| !generation.is_empty())
        else {
            continue;
        };
        let generation = IntegrationSecretGeneration::new(guild, module.clone(), id, generation)
            .map_err(|_| {
                ApiError::Internal("the stored ticket integration has an invalid generation".into())
            })?;
        if generations.insert(id.to_owned(), generation).is_some() {
            return Err(ApiError::Internal(
                "the stored ticket configuration has duplicate integration IDs".into(),
            ));
        }
    }
    Ok(generations)
}

fn valid_integration_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn stronger_role_added(
    current: &GuildOverrides,
    next: &GuildOverrides,
    level: u8,
    permissions: &closure_core::permission::GuildPermissions,
) -> Result<bool, ApiError> {
    fn references(document: &GuildOverrides) -> Result<BTreeSet<u64>, ApiError> {
        fn visit(value: &serde_json::Value, ids: &mut BTreeSet<u64>) {
            match value {
                serde_json::Value::String(s) => {
                    if let Ok(id) = s.parse::<u64>() {
                        ids.insert(id);
                    }
                }
                serde_json::Value::Number(n) => {
                    if let Some(id) = n.as_u64() {
                        ids.insert(id);
                    }
                }
                serde_json::Value::Array(values) => values.iter().for_each(|v| visit(v, ids)),
                serde_json::Value::Object(values) => {
                    for (key, value) in values {
                        if let Ok(id) = key.parse::<u64>() {
                            ids.insert(id);
                        }
                        visit(value, ids);
                    }
                }
                _ => {}
            }
        }
        let mut value =
            serde_json::to_value(document).map_err(|e| ApiError::Internal(e.to_string()))?;
        if let Some(object) = value.as_object_mut() {
            for field in PRIVILEGED_FIELDS {
                object.remove(field);
            }
        }
        let mut ids = BTreeSet::new();
        visit(&value, &mut ids);
        Ok(ids)
    }
    let before = references(current)?;
    let after = references(next)?;
    let Some(saved) = &current.permissions else {
        return Ok(false);
    };
    Ok(after.difference(&before).any(|id| {
        saved.roles.keys().any(|role| {
            role.parse::<u64>().ok() == Some(*id)
                && permissions
                    .role_levels
                    .iter()
                    .any(|(role, role_level)| role.get() == *id && role_level.get() > level)
        }) || saved.keys.values().any(|rule| {
            rule.allow
                .as_ref()
                .and_then(|allow| allow.roles.as_ref())
                .is_some_and(|roles| roles.iter().any(|role| role.get() == *id))
        })
    }))
}

fn privileged_changed(current: &GuildOverrides, next: &GuildOverrides) -> bool {
    PRIVILEGED_FIELDS.iter().any(|field| match *field {
        "permissions" => current.permissions != next.permissions,
        "moderator_roles" => current.moderator_roles != next.moderator_roles,
        _ => false,
    })
}

/// The write both views share: validate, check the server, store, install and audit.
#[allow(clippy::too_many_arguments)]
async fn save(
    state: &ApiState,
    guild: GuildId,
    expected: Option<i64>,
    overrides: GuildOverrides,
    actor: Option<UserId>,
    active_generations: &[IntegrationSecretGeneration],
    retirements: &[IntegrationSecretGeneration],
    can_retire_secrets: bool,
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
        .save_config_and_retire_integration_generations(
            guild,
            expected,
            &overrides,
            actor,
            active_generations,
            retirements,
            can_retire_secrets,
        )
        .await?;
    if !state.inner.settings.install(guild, revision, settings) {
        // A concurrent edit with a higher revision is already live; that is the one to keep.
        tracing::debug!(%guild, revision, "a newer configuration was already installed");
    }

    let audit = AuditEvent {
        guild_id: guild,
        actor,
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
    /// Only the revisions older than this one: the next page of a walk back.
    before: Option<i64>,
}

#[derive(Serialize)]
struct HistoryEntry {
    revision: i64,
    created_at_unix: i64,
    actor: Option<String>,
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
        .config_history_before(guild, query.before, query.limit.unwrap_or(20))
        .await?;
    let body: Vec<HistoryEntry> = entries
        .into_iter()
        .map(|entry| HistoryEntry {
            revision: entry.revision,
            created_at_unix: entry.created_at.unix_timestamp(),
            actor: entry.actor.map(|id| id.to_string()),
            document: entry.overrides,
        })
        .collect();
    Ok((StatusCode::OK, Json(body)).into_response())
}

#[derive(Serialize)]
struct OrphanSecretList {
    orphans: Vec<OrphanSecret>,
}

#[derive(Serialize)]
struct OrphanSecret {
    integration_id: String,
    generation: String,
    secret_name: String,
    updated_at_unix: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OrphanSecretTarget {
    integration_id: String,
    generation: String,
    secret_name: String,
}

/// Only the owner or an administrator may list or delete stored integration secrets.
async fn secret_manager(
    state: &ApiState,
    guild: GuildId,
    headers: &HeaderMap,
) -> Result<UserId, ApiError> {
    let (_, viewer) = crate::tickets::viewer(state, guild.get(), headers).await?;
    if viewer.actor.owner || viewer.actor.administrator {
        Ok(viewer.actor.user)
    } else {
        Err(ApiError::Forbidden(
            "a guild owner or administrator must manage integration secrets".into(),
        ))
    }
}

/// Lists the stored secret values that no saved integration names: their places only, never values.
pub(crate) async fn list_orphan_secrets(
    State(state): State<ApiState>,
    Path(guild): Path<u64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let guild = guild_id(guild)?;
    secret_manager(&state, guild, &headers).await?;
    let orphans = state
        .inner
        .db
        .list_orphan_integration_secrets(guild)
        .await?;
    let body = OrphanSecretList {
        orphans: orphans
            .into_iter()
            .map(|orphan| OrphanSecret {
                integration_id: orphan.integration_id,
                generation: orphan.generation,
                secret_name: orphan.secret_name,
                updated_at_unix: orphan.updated_at.unix_timestamp(),
            })
            .collect(),
    };
    Ok(Json(body).into_response())
}

/// Whether a target names a place a stored integration secret can have.
fn valid_orphan_target(target: &OrphanSecretTarget) -> bool {
    valid_integration_id(&target.integration_id)
        && target.generation.len() == 32
        && target
            .generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && (1..=64).contains(&target.secret_name.len())
        && target.secret_name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

/// Deletes one stored secret value that no saved integration names. The body is
/// `{"integration_id": "...", "generation": "...", "secret_name": "..."}`.
pub(crate) async fn delete_orphan_secret(
    State(state): State<ApiState>,
    Path(guild): Path<u64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let guild = guild_id(guild)?;
    let actor = secret_manager(&state, guild, &headers).await?;
    let target: OrphanSecretTarget = serde_json::from_slice(&body).map_err(|_| {
        ApiError::BadRequest("the body must name integration_id, generation and secret_name".into())
    })?;
    if !valid_orphan_target(&target) {
        return Err(ApiError::BadRequest(
            "the integration secret target is malformed".into(),
        ));
    }
    let deleted = state
        .inner
        .db
        .delete_orphan_integration_secret(
            guild,
            &target.integration_id,
            &target.generation,
            &target.secret_name,
        )
        .await?;
    if !deleted {
        return Err(ApiError::NotFound(
            "no stored value of that name is an orphan".into(),
        ));
    }
    let audit = AuditEvent {
        guild_id: guild,
        actor: Some(actor),
        action: "integration_secret.orphan_delete".into(),
        payload: json!({
            "integration_id": target.integration_id,
            "generation": target.generation,
            "secret_name": target.secret_name,
        }),
    };
    if let Err(error) = state.inner.db.record_audit(&audit).await {
        tracing::warn!(%error, %guild, "could not record the orphan secret deletion in the audit trail");
    }
    Ok(Json(json!({ "deleted": true })).into_response())
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
    fn orphan_targets_must_name_a_well_formed_place() {
        let target =
            |integration_id: &str, generation: &str, secret_name: &str| OrphanSecretTarget {
                integration_id: integration_id.into(),
                generation: generation.into(),
                secret_name: secret_name.into(),
            };
        let generation = "0123456789abcdef0123456789abcdef";
        assert!(valid_orphan_target(&target("crm", generation, "api-token")));
        assert!(!valid_orphan_target(&target(
            "CRM",
            generation,
            "api-token"
        )));
        assert!(!valid_orphan_target(&target("crm", "0123", "api-token")));
        assert!(!valid_orphan_target(&target("crm", generation, "")));
        assert!(!valid_orphan_target(&target(
            "crm",
            generation,
            "Api Token"
        )));
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
