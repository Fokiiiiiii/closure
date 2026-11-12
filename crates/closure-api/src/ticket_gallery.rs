use std::collections::HashSet;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
};
use closure_core::module::{DashboardActionError, DashboardActionRequest, DashboardScope};
use closure_db::{
    GalleryCard, GalleryError, GalleryFilter, GalleryPage, GallerySubmission, GalleryTemplate,
    NewGalleryTemplate,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::{ApiState, config::actor, error::ApiError, tickets};

const MAX_OFFSET: u32 = 10_000;
const DEFAULT_LIMIT: u16 = 24;
const MAX_LIMIT: u16 = 50;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GalleryQuery {
    kind: Option<String>,
    category: Option<String>,
    q: Option<String>,
    sort: Option<String>,
    offset: Option<u32>,
    limit: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GallerySubmitBody {
    kind: String,
    source_id: String,
    title: String,
    description: String,
    category: String,
    #[serde(default)]
    tags: Vec<String>,
    author_name: Option<String>,
    consent: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GallerySubmissionActionBody {
    action: String,
    revision: i64,
    kind: Option<String>,
    source_id: Option<String>,
    title: Option<String>,
    description: Option<String>,
    category: Option<String>,
    tags: Option<Vec<String>>,
    author_name: Option<String>,
    consent: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GalleryReviewBody {
    status: String,
    revision: i64,
    reason: Option<String>,
    #[serde(default)]
    featured: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyBody {}

pub(crate) async fn templates(
    State(state): State<ApiState>,
    Path(raw_guild): Path<u64>,
    headers: HeaderMap,
    Query(query): Query<GalleryQuery>,
) -> Result<Response, ApiError> {
    let _ = tickets::viewer(&state, raw_guild, &headers).await?;
    let kind = query
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if kind.is_some_and(|kind| !matches!(kind, "panel" | "tag" | "form")) {
        return Err(ApiError::BadRequest("template type is invalid".into()));
    }
    let category = query
        .category
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if category.is_some_and(|category| !valid_category(category)) {
        return Err(ApiError::BadRequest("template category is invalid".into()));
    }
    let query_text = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if query_text
        .is_some_and(|value| value.chars().count() > 100 || value.chars().any(char::is_control))
    {
        return Err(ApiError::BadRequest("search query is invalid".into()));
    }
    let query_text = query_text.map(escape_like);
    let sort = query.sort.as_deref().unwrap_or("newest");
    if !matches!(sort, "popular" | "newest") {
        return Err(ApiError::BadRequest("template sort is invalid".into()));
    }
    let offset = query.offset.unwrap_or(0);
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if offset > MAX_OFFSET || !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest("gallery page is invalid".into()));
    }
    let page = state
        .inner
        .db
        .ticket_gallery_list(&GalleryFilter {
            kind,
            category,
            query: query_text.as_deref(),
            sort,
            offset: i64::from(offset),
            limit: i64::from(limit),
        })
        .await
        .map_err(gallery_error)?;
    Ok(no_store(Json(gallery_page_json(page)).into_response()))
}

pub(crate) async fn template(
    State(state): State<ApiState>,
    Path((raw_guild, raw_id)): Path<(u64, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let _ = tickets::viewer(&state, raw_guild, &headers).await?;
    let id = template_id(&raw_id)?;
    let template = state
        .inner
        .db
        .ticket_gallery_template(id)
        .await
        .map_err(gallery_error)?
        .ok_or_else(|| ApiError::NotFound("gallery template not found".into()))?;
    Ok(no_store(
        Json(json!({ "template": template_json(template) })).into_response(),
    ))
}

pub(crate) async fn submissions(
    State(state): State<ApiState>,
    Path(raw_guild): Path<u64>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (_, viewer) = tickets::viewer(&state, raw_guild, &headers).await?;
    require_manage(viewer.can_manage)?;
    let actor_id = user_db_id(viewer.actor.user.get())?;
    let submissions = state
        .inner
        .db
        .ticket_gallery_submissions(actor_id)
        .await
        .map_err(gallery_error)?;
    Ok(no_store(
        Json(json!({
            "submissions": submissions.into_iter().map(submission_json).collect::<Vec<_>>()
        }))
        .into_response(),
    ))
}

pub(crate) async fn submit(
    State(state): State<ApiState>,
    Path(raw_guild): Path<u64>,
    headers: HeaderMap,
    Json(body): Json<GallerySubmitBody>,
) -> Result<Response, ApiError> {
    let (guild, viewer) = tickets::viewer(&state, raw_guild, &headers).await?;
    require_manage(viewer.can_manage)?;
    if !body.consent {
        return Err(ApiError::BadRequest(
            "gallery sharing consent is required".into(),
        ));
    }
    let key = request_key(&headers)?;
    let source_guild_id = i64::try_from(guild.get())
        .map_err(|_| ApiError::BadRequest("guild ID is invalid".into()))?;
    let (template, source_id) = validated_source_request(
        body.kind,
        body.source_id,
        body.title,
        body.description,
        body.category,
        body.tags,
        body.author_name,
        source_guild_id,
    )?;
    let payload = gallery_source_payload(
        &state,
        guild,
        viewer.actor.clone(),
        &template.kind,
        &source_id,
        &key,
    )
    .await?;
    let input = NewGalleryTemplate {
        payload: payload.clone(),
        ..template
    };
    let fingerprint = fingerprint(&json!({
        "action": "submit",
        "guild_id": guild.get(),
        "source_id": source_id,
        "kind": input.kind,
        "title": input.title,
        "description": input.description,
        "category": input.category,
        "tags": input.tags,
        "author_name": input.author_name,
        "payload": payload,
    }))?;
    let actor_id = user_db_id(viewer.actor.user.get())?;
    let submission = state
        .inner
        .db
        .ticket_gallery_submit(actor_id, &key, &fingerprint, &input)
        .await
        .map_err(gallery_error)?;
    Ok(no_store(
        Json(json!({ "submission": submission_json(submission) })).into_response(),
    ))
}

pub(crate) async fn submission_action(
    State(state): State<ApiState>,
    Path((raw_guild, raw_id)): Path<(u64, String)>,
    headers: HeaderMap,
    Json(body): Json<GallerySubmissionActionBody>,
) -> Result<Response, ApiError> {
    let (guild, viewer) = tickets::viewer(&state, raw_guild, &headers).await?;
    require_manage(viewer.can_manage)?;
    let id = template_id(&raw_id)?;
    if body.revision <= 0 {
        return Err(ApiError::BadRequest(
            "submission revision is invalid".into(),
        ));
    }
    let key = request_key(&headers)?;
    let actor_id = user_db_id(viewer.actor.user.get())?;
    let source_guild_id = i64::try_from(guild.get())
        .map_err(|_| ApiError::BadRequest("guild ID is invalid".into()))?;
    let submission = match body.action.as_str() {
        "update" => {
            if body.consent != Some(true) {
                return Err(ApiError::BadRequest(
                    "gallery sharing consent is required".into(),
                ));
            }
            let (template, source_id) = validated_source_request(
                body.kind
                    .ok_or_else(|| ApiError::BadRequest("template type is required".into()))?,
                body.source_id
                    .ok_or_else(|| ApiError::BadRequest("template source is required".into()))?,
                body.title
                    .ok_or_else(|| ApiError::BadRequest("template title is required".into()))?,
                body.description.ok_or_else(|| {
                    ApiError::BadRequest("template description is required".into())
                })?,
                body.category
                    .ok_or_else(|| ApiError::BadRequest("template category is required".into()))?,
                body.tags
                    .ok_or_else(|| ApiError::BadRequest("template tags are required".into()))?,
                body.author_name,
                source_guild_id,
            )?;
            let payload = gallery_source_payload(
                &state,
                guild,
                viewer.actor.clone(),
                &template.kind,
                &source_id,
                &key,
            )
            .await?;
            let update = NewGalleryTemplate {
                payload: payload.clone(),
                ..template
            };
            let fingerprint = fingerprint(&json!({
                "action": "update",
                "submission_id": id,
                "revision": body.revision,
                "guild_id": guild.get(),
                "source_id": source_id,
                "kind": update.kind,
                "title": update.title,
                "description": update.description,
                "category": update.category,
                "tags": update.tags,
                "author_name": update.author_name,
                "payload": payload,
            }))?;
            state
                .inner
                .db
                .ticket_gallery_update(actor_id, id, body.revision, &key, &fingerprint, &update)
                .await
                .map_err(gallery_error)?
        }
        "withdraw" | "unpublish" => {
            if body.kind.is_some()
                || body.source_id.is_some()
                || body.title.is_some()
                || body.description.is_some()
                || body.category.is_some()
                || body.tags.is_some()
                || body.author_name.is_some()
                || body.consent.is_some()
            {
                return Err(ApiError::BadRequest(
                    "submission action has unexpected fields".into(),
                ));
            }
            let fingerprint = fingerprint(&json!({
                "action": body.action,
                "submission_id": id,
                "revision": body.revision,
            }))?;
            state
                .inner
                .db
                .ticket_gallery_change_submission(
                    actor_id,
                    id,
                    body.revision,
                    &key,
                    &fingerprint,
                    &body.action,
                )
                .await
                .map_err(gallery_error)?
        }
        _ => return Err(ApiError::BadRequest("submission action is invalid".into())),
    };
    Ok(no_store(
        Json(json!({ "submission": submission_json(submission) })).into_response(),
    ))
}

pub(crate) async fn record_import(
    State(state): State<ApiState>,
    Path((raw_guild, raw_id)): Path<(u64, String)>,
    headers: HeaderMap,
    Json(_body): Json<EmptyBody>,
) -> Result<Response, ApiError> {
    let (guild, viewer) = tickets::viewer(&state, raw_guild, &headers).await?;
    require_manage(viewer.can_manage)?;
    let id = template_id(&raw_id)?;
    let key = request_key(&headers)?;
    let actor_id = user_db_id(viewer.actor.user.get())?;
    let fingerprint = fingerprint(&json!({
        "action": "import", "template_id": id, "guild_id": guild.get()
    }))?;
    let import_count = state
        .inner
        .db
        .ticket_gallery_record_import(actor_id, id, &key, &fingerprint)
        .await
        .map_err(gallery_error)?;
    Ok(no_store(
        Json(json!({ "import_count": import_count })).into_response(),
    ))
}

pub(crate) async fn review(
    State(state): State<ApiState>,
    Path(raw_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<GalleryReviewBody>,
) -> Result<Response, ApiError> {
    let reviewer = actor(&headers)?
        .ok_or_else(|| ApiError::Forbidden("a verified moderator is required".into()))?;
    if !gallery_moderator(reviewer.get()) {
        return Err(ApiError::Forbidden(
            "a platform moderator is required".into(),
        ));
    }
    let id = template_id(&raw_id)?;
    if body.revision <= 0 || !matches!(body.status.as_str(), "approved" | "rejected") {
        return Err(ApiError::BadRequest("gallery review is invalid".into()));
    }
    let reason = match (body.status.as_str(), body.reason) {
        ("approved", None) => None,
        ("approved", Some(reason)) if reason.trim().is_empty() => None,
        ("approved", Some(_)) => {
            return Err(ApiError::BadRequest(
                "approved reviews cannot include a rejection reason".into(),
            ));
        }
        ("rejected", Some(reason)) => Some(valid_text(reason, "review reason", 500)?),
        ("rejected", None) => {
            return Err(ApiError::BadRequest(
                "a rejection reason is required".into(),
            ));
        }
        _ => unreachable!(),
    };
    let featured = body.featured && body.status == "approved";
    let key = request_key(&headers)?;
    let reviewer_id = user_db_id(reviewer.get())?;
    let fingerprint = fingerprint(&json!({
        "action": format!("review_{}", body.status),
        "template_id": id,
        "revision": body.revision,
        "reason": reason,
        "featured": featured,
    }))?;
    let submission = state
        .inner
        .db
        .ticket_gallery_review(
            reviewer_id,
            id,
            body.revision,
            &key,
            &fingerprint,
            &body.status,
            reason.as_deref(),
            featured,
        )
        .await
        .map_err(gallery_error)?;
    Ok(no_store(
        Json(json!({
            "submission": {
                "id": submission.id,
                "status": submission.status,
                "revision": submission.revision,
                "featured": submission.featured,
            }
        }))
        .into_response(),
    ))
}

async fn gallery_source_payload(
    state: &ApiState,
    guild: closure_core::event::GuildId,
    actor: closure_core::permission::Actor,
    kind: &str,
    source_id: &str,
    request_key: &str,
) -> Result<Value, ApiError> {
    let sink = state.ticket_actions.as_ref().ok_or(ApiError::Unavailable)?;
    let mut export_key = String::from("e");
    for byte in Sha256::digest(request_key.as_bytes()).iter().take(31) {
        export_key.push_str(&format!("{byte:02x}"));
    }
    let result = sink
        .perform(
            guild,
            actor,
            DashboardActionRequest {
                scope: DashboardScope::Gallery,
                action: "gallery.export".into(),
                data: json!({ "kind": kind, "source_id": source_id }),
                expected_revision: 0,
                idempotency_key: export_key,
            },
        )
        .await
        .map_err(dashboard_error)?;
    if result.value.get("kind").and_then(Value::as_str) != Some(kind) {
        return Err(ApiError::Internal("gallery export type is invalid".into()));
    }
    let payload = result
        .value
        .get("payload")
        .ok_or_else(|| ApiError::Internal("gallery export has no payload".into()))?;
    sanitize_payload(kind, payload)
}

fn dashboard_error(error: DashboardActionError) -> ApiError {
    match error {
        DashboardActionError::Unsupported => {
            ApiError::BadRequest("gallery operation is unsupported".into())
        }
        DashboardActionError::Invalid(message) => ApiError::BadRequest(message),
        DashboardActionError::NotFound => ApiError::NotFound("gallery source not found".into()),
        DashboardActionError::Forbidden => {
            ApiError::Forbidden("ticket management permission is required".into())
        }
        DashboardActionError::Conflict => ApiError::Conflict("gallery source changed".into()),
        DashboardActionError::PreconditionFailed { current } => {
            ApiError::PreconditionFailed { current }
        }
        DashboardActionError::Unavailable => ApiError::Unavailable,
        DashboardActionError::Internal(_) => {
            ApiError::Internal("gallery source is unavailable".into())
        }
    }
}

fn gallery_error(error: GalleryError) -> ApiError {
    match error {
        GalleryError::NotFound => ApiError::NotFound("gallery item not found".into()),
        GalleryError::Conflict => {
            ApiError::Conflict("gallery item changed; reload and retry".into())
        }
        GalleryError::Database(error) => error.into(),
    }
}

fn gallery_page_json(page: GalleryPage) -> Value {
    json!({
        "featured": page.featured.iter().map(card_json).collect::<Vec<_>>(),
        "templates": page.templates.iter().map(card_json).collect::<Vec<_>>(),
        "total": page.total,
        "offset": page.offset,
        "next_offset": page.next_offset,
    })
}

fn card_json(card: &GalleryCard) -> Value {
    json!({
        "id": card.id,
        "kind": card.kind,
        "title": card.title,
        "description": card.description,
        "category": card.category,
        "tags": card.tags,
        "author_name": card.author_name,
        "import_count": card.import_count,
        "created_at": card.created_at,
        "featured": card.featured,
    })
}

fn template_json(template: GalleryTemplate) -> Value {
    let mut value = card_json(&template.card);
    if let Value::Object(map) = &mut value {
        map.insert("payload".into(), template.payload);
    }
    value
}

fn submission_json(submission: GallerySubmission) -> Value {
    json!({
        "id": submission.id,
        "kind": submission.kind,
        "title": submission.title,
        "description": submission.description,
        "category": submission.category,
        "tags": submission.tags,
        "payload": submission.payload,
        "author_name": submission.author_name,
        "status": submission.status,
        "review_reason": submission.review_reason,
        "featured": submission.featured,
        "import_count": submission.import_count,
        "revision": submission.revision,
        "created_at": submission.created_at,
        "updated_at": submission.updated_at,
        "published_at": submission.published_at,
    })
}

fn validated_source_request(
    kind: String,
    source_id: String,
    title: String,
    description: String,
    category: String,
    tags: Vec<String>,
    author_name: Option<String>,
    source_guild_id: i64,
) -> Result<(NewGalleryTemplate, String), ApiError> {
    let kind = kind.trim().to_owned();
    if !matches!(kind.as_str(), "panel" | "tag" | "form") {
        return Err(ApiError::BadRequest("template type is invalid".into()));
    }
    if source_id.is_empty()
        || source_id.len() > 64
        || !source_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(ApiError::BadRequest("template source is invalid".into()));
    }
    let title = valid_text(title, "template title", 100)?;
    let description = valid_text(description, "template description", 500)?;
    let category = category.trim().to_ascii_lowercase();
    if !valid_category(&category) {
        return Err(ApiError::BadRequest("template category is invalid".into()));
    }
    if tags.len() > 3 {
        return Err(ApiError::BadRequest(
            "at most three template tags are allowed".into(),
        ));
    }
    let mut normalized_tags = Vec::with_capacity(tags.len());
    let mut seen = HashSet::new();
    for tag in tags {
        let tag = valid_text(tag, "template tag", 24)?.to_ascii_lowercase();
        if !tag
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
            || !seen.insert(tag.clone())
        {
            return Err(ApiError::BadRequest("template tag is invalid".into()));
        }
        normalized_tags.push(tag);
    }
    let author_name = match author_name {
        Some(name) if !name.trim().is_empty() => valid_text(name, "author name", 80)?,
        _ => "Community member".to_owned(),
    };
    Ok((
        NewGalleryTemplate {
            kind,
            title,
            description,
            category,
            tags: normalized_tags,
            payload: Value::Null,
            author_name,
            source_guild_id,
        },
        source_id,
    ))
}

fn valid_category(category: &str) -> bool {
    matches!(
        category,
        "support" | "moderation" | "sales" | "applications" | "feedback" | "general" | "other"
    )
}

fn valid_text(value: String, label: &str, max_chars: usize) -> Result<String, ApiError> {
    let value = value.trim().to_owned();
    if value.is_empty()
        || value.chars().count() > max_chars
        || value.chars().any(|ch| ch.is_control())
        || has_server_reference(&value)
    {
        return Err(ApiError::BadRequest(format!("{label} is invalid")));
    }
    Ok(value)
}

fn has_forbidden_controls(value: &str) -> bool {
    value
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
}

fn has_server_reference(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "<@",
        "<#",
        "<:",
        "<a:",
        "discord.com/channels/",
        "discordapp.com/channels/",
        "discord.gg/",
        "discord.com/invite/",
        "discordapp.com/attachments/",
        "cdn.discordapp.com/attachments/",
        "media.discordapp.net/attachments/",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn template_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| ApiError::BadRequest("template ID is invalid".into()))
}

fn user_db_id(id: u64) -> Result<i64, ApiError> {
    i64::try_from(id).map_err(|_| ApiError::BadRequest("actor ID is invalid".into()))
}

fn require_manage(can_manage: bool) -> Result<(), ApiError> {
    if can_manage {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "ticket management permission is required".into(),
        ))
    }
}

fn request_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let key = values
        .next()
        .and_then(|value| value.to_str().ok())
        .filter(|key| (1..=64).contains(&key.len()))
        .filter(|key| {
            key.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .ok_or_else(|| ApiError::BadRequest("Idempotency-Key is required".into()))?;
    if values.next().is_some() {
        return Err(ApiError::BadRequest(
            "Idempotency-Key must be sent once".into(),
        ));
    }
    Ok(key.to_owned())
}

fn fingerprint(value: &Value) -> Result<[u8; 32], ApiError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| ApiError::BadRequest("request is invalid".into()))?;
    Ok(Sha256::digest(bytes).into())
}

fn gallery_moderator(actor_id: u64) -> bool {
    let Ok(raw) = std::env::var("CLOSURE_GALLERY_MODERATORS") else {
        return false;
    };
    let values: Vec<&str> = raw.split(',').map(str::trim).collect();
    if values.is_empty() || values.iter().any(|value| value.is_empty()) {
        return false;
    }
    let mut moderators = Vec::with_capacity(values.len());
    for value in values {
        let Some(id) = value
            .bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| value.parse::<u64>().ok())
            .flatten()
            .filter(|id| *id > 0 && i64::try_from(*id).is_ok())
        else {
            return false;
        };
        moderators.push(id);
    }
    moderators.contains(&actor_id)
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn sanitize_payload(kind: &str, payload: &Value) -> Result<Value, ApiError> {
    match kind {
        "panel" => sanitize_panel(payload),
        "tag" => sanitize_tag(payload),
        "form" => sanitize_form(payload),
        _ => Err(ApiError::BadRequest("template type is invalid".into())),
    }
}

fn only_keys<'a>(
    value: &'a Value,
    allowed: &[&str],
    label: &str,
) -> Result<&'a Map<String, Value>, ApiError> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::BadRequest(format!("{label} is invalid")))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ApiError::BadRequest(format!(
            "{label} contains unsupported fields"
        )));
    }
    Ok(object)
}

fn safe_optional_text(
    object: &Map<String, Value>,
    key: &str,
    max_chars: usize,
) -> Result<Option<String>, ApiError> {
    object
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            let text = value
                .as_str()
                .ok_or_else(|| ApiError::BadRequest(format!("template {key} is invalid")))?;
            if text.chars().count() > max_chars
                || has_forbidden_controls(text)
                || has_server_reference(text)
            {
                return Err(ApiError::BadRequest(format!("template {key} is invalid")));
            }
            Ok(text.to_owned())
        })
        .transpose()
}

fn safe_optional_url(object: &Map<String, Value>, key: &str) -> Result<Option<String>, ApiError> {
    let value = safe_optional_text(object, key, 2048)?;
    if value.as_deref().is_some_and(|value| !safe_https_url(value)) {
        return Err(ApiError::BadRequest(format!(
            "template {key} must be a public HTTPS URL"
        )));
    }
    Ok(value)
}

fn safe_https_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://") else {
        return false;
    };
    if value.len() > 2048
        || value.chars().any(|ch| matches!(ch, '?' | '#' | '@' | '\\'))
        || value.chars().any(char::is_whitespace)
        || has_server_reference(value)
    {
        return false;
    }
    let host = rest.split('/').next().unwrap_or_default();
    let host = host.split(':').next().unwrap_or_default();
    !host.is_empty()
        && host.contains('.')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn safe_color(value: &Value, label: &str) -> Result<Value, ApiError> {
    if value.as_u64().is_some_and(|color| color <= 0xFF_FFFF) {
        return Ok(value.clone());
    }
    if let Some(color) = value.as_str() {
        let value = color.strip_prefix('#').unwrap_or(color);
        if value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(Value::String(format!("#{value}")));
        }
    }
    Err(ApiError::BadRequest(format!("template {label} is invalid")))
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, ApiError> {
    object
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| ApiError::BadRequest(format!("template {key} is invalid")))
        })
        .transpose()
}

fn sanitize_panel(payload: &Value) -> Result<Value, ApiError> {
    let object = only_keys(
        payload,
        &["id", "name", "description", "button_style", "customization"],
        "panel template",
    )?;
    let name = safe_optional_text(object, "name", 80)?;
    let description = safe_optional_text(object, "description", 256)?;
    if name.as_deref().is_some_and(|name| name.trim().is_empty())
        || description
            .as_deref()
            .is_some_and(|description| description.trim().is_empty())
    {
        return Err(ApiError::BadRequest(
            "panel name or description is invalid".into(),
        ));
    }
    let style = object
        .get("button_style")
        .filter(|style| !style.is_null())
        .map(|style| {
            style
                .as_str()
                .filter(|style| matches!(*style, "primary" | "secondary" | "success" | "danger"))
                .ok_or_else(|| ApiError::BadRequest("panel button style is invalid".into()))
        })
        .transpose()?;
    let customization = object
        .get("customization")
        .ok_or_else(|| ApiError::BadRequest("panel customization is invalid".into()))?;
    let source = only_keys(
        customization,
        &[
            "title",
            "color",
            "content",
            "welcome_title",
            "welcome_title_url",
            "welcome_color",
            "welcome_description",
            "welcome_author_name",
            "welcome_author_url",
            "welcome_author_icon_url",
            "welcome_thumbnail_url",
            "welcome_image_url",
            "welcome_footer_text",
            "welcome_footer_icon_url",
            "welcome_timestamp",
            "welcome_fields",
        ],
        "panel customization",
    )?;
    let mut safe = Map::new();
    for (key, max) in [
        ("title", 256),
        ("content", 4000),
        ("welcome_title", 256),
        ("welcome_description", 4000),
        ("welcome_author_name", 256),
        ("welcome_footer_text", 2048),
    ] {
        if let Some(value) = safe_optional_text(source, key, max)? {
            safe.insert(key.to_owned(), Value::String(value));
        }
    }
    for key in ["color", "welcome_color"] {
        if let Some(value) = source.get(key).filter(|value| !value.is_null()) {
            safe.insert(key.to_owned(), safe_color(value, key)?);
        }
    }
    for key in [
        "welcome_title_url",
        "welcome_author_url",
        "welcome_author_icon_url",
        "welcome_thumbnail_url",
        "welcome_image_url",
        "welcome_footer_icon_url",
    ] {
        if let Some(value) = safe_optional_url(source, key)? {
            safe.insert(key.to_owned(), Value::String(value));
        }
    }
    if let Some(value) = optional_bool(source, "welcome_timestamp")? {
        safe.insert("welcome_timestamp".into(), Value::Bool(value));
    }
    if let Some(fields) = source.get("welcome_fields") {
        safe.insert("welcome_fields".into(), sanitize_embed_fields(fields)?);
    }
    let mut panel = Map::new();
    if let Some(name) = name {
        panel.insert("name".into(), Value::String(name));
    }
    if let Some(description) = description {
        panel.insert("description".into(), Value::String(description));
    }
    if let Some(style) = style {
        panel.insert("button_style".into(), Value::String(style.to_owned()));
    }
    panel.insert("customization".into(), Value::Object(safe));
    Ok(Value::Object(panel))
}

fn sanitize_tag(payload: &Value) -> Result<Value, ApiError> {
    let object = only_keys(
        payload,
        &["id", "content", "use_embed", "embed"],
        "tag template",
    )?;
    let content = safe_optional_text(object, "content", 2000)?.unwrap_or_default();
    let use_embed = object
        .get("use_embed")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::BadRequest("tag embed setting is invalid".into()))?;
    let embed = match object.get("embed").filter(|embed| !embed.is_null()) {
        Some(embed) => sanitize_tag_embed(embed)?,
        None if use_embed => return Err(ApiError::BadRequest("tag embed is missing".into())),
        None => Value::Null,
    };
    if content.trim().is_empty() && !use_embed {
        return Err(ApiError::BadRequest("tag template has no content".into()));
    }
    Ok(json!({
        "content": content,
        "use_embed": use_embed,
        "embed": embed,
    }))
}

fn sanitize_tag_embed(value: &Value) -> Result<Value, ApiError> {
    let source = only_keys(
        value,
        &[
            "title",
            "color",
            "description",
            "author_name",
            "author_url",
            "author_icon_url",
            "thumbnail_url",
            "image_url",
            "footer_text",
            "footer_icon_url",
            "footer_timestamp",
            "fields",
        ],
        "tag embed",
    )?;
    let mut safe = Map::new();
    for (key, max) in [
        ("title", 256),
        ("description", 4096),
        ("author_name", 256),
        ("footer_text", 2048),
    ] {
        if let Some(value) = safe_optional_text(source, key, max)? {
            safe.insert(key.to_owned(), Value::String(value));
        }
    }
    if let Some(color) = source.get("color").filter(|color| !color.is_null()) {
        safe.insert("color".into(), safe_color(color, "tag embed color")?);
    }
    for key in [
        "author_url",
        "author_icon_url",
        "thumbnail_url",
        "image_url",
        "footer_icon_url",
    ] {
        if let Some(value) = safe_optional_url(source, key)? {
            safe.insert(key.to_owned(), Value::String(value));
        }
    }
    if let Some(value) = optional_bool(source, "footer_timestamp")? {
        safe.insert("footer_timestamp".into(), Value::Bool(value));
    }
    if let Some(fields) = source.get("fields") {
        safe.insert("fields".into(), sanitize_embed_fields(fields)?);
    }
    Ok(Value::Object(safe))
}

fn sanitize_embed_fields(fields: &Value) -> Result<Value, ApiError> {
    let fields = fields
        .as_array()
        .filter(|fields| fields.len() <= 25)
        .ok_or_else(|| ApiError::BadRequest("template embed fields are invalid".into()))?;
    let mut safe = Vec::with_capacity(fields.len());
    for field in fields {
        let object = only_keys(field, &["name", "value", "inline"], "embed field")?;
        let name = safe_optional_text(object, "name", 256)?
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| ApiError::BadRequest("embed field name is invalid".into()))?;
        let value = safe_optional_text(object, "value", 1024)?
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ApiError::BadRequest("embed field value is invalid".into()))?;
        let inline = optional_bool(object, "inline")?.unwrap_or(false);
        safe.push(json!({ "name": name, "value": value, "inline": inline }));
    }
    Ok(Value::Array(safe))
}

fn sanitize_form(payload: &Value) -> Result<Value, ApiError> {
    let object = only_keys(payload, &["title", "questions"], "form template")?;
    let title = safe_optional_text(object, "title", 100)?
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| ApiError::BadRequest("form title is invalid".into()))?;
    let questions = object
        .get("questions")
        .and_then(Value::as_array)
        .filter(|questions| questions.len() <= 5)
        .ok_or_else(|| ApiError::BadRequest("form questions are invalid".into()))?;
    let safe_questions = questions
        .iter()
        .map(sanitize_question)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({ "title": title, "questions": safe_questions }))
}

fn sanitize_question(value: &Value) -> Result<Value, ApiError> {
    if let Some(text) = value.as_str() {
        if text.trim().is_empty() || text.chars().count() > 45 || has_forbidden_controls(text) {
            return Err(ApiError::BadRequest("form question is invalid".into()));
        }
        return Ok(value.clone());
    }
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::BadRequest("form question is invalid".into()))?;
    if object.contains_key("dynamic_options") {
        return Err(ApiError::BadRequest(
            "dynamic form options cannot be shared".into(),
        ));
    }
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::BadRequest("form question type is invalid".into()))?;
    let allowed: &[&str] = match kind {
        "text" => &[
            "type",
            "label",
            "style",
            "required",
            "min_length",
            "max_length",
            "placeholder",
            "description",
        ],
        "string_select" => &[
            "type",
            "label",
            "description",
            "required",
            "options",
            "min_values",
            "max_values",
        ],
        "user_select" | "role_select" | "mentionable_select" | "channel_select" => &[
            "type",
            "label",
            "description",
            "required",
            "min_values",
            "max_values",
        ],
        "radio_group" => &["type", "label", "description", "required", "options"],
        "checkbox_group" => &[
            "type",
            "label",
            "description",
            "required",
            "options",
            "min_values",
            "max_values",
        ],
        "display" => &["type", "content"],
        _ => return Err(ApiError::BadRequest("form question type is invalid".into())),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ApiError::BadRequest(
            "form question contains unsupported fields".into(),
        ));
    }
    if kind == "display" {
        let content = safe_optional_text(object, "content", 4000)?
            .filter(|content| !content.trim().is_empty())
            .ok_or_else(|| ApiError::BadRequest("form display content is invalid".into()))?;
        return Ok(json!({ "type": kind, "content": content }));
    }
    let label = safe_optional_text(object, "label", 45)?
        .filter(|label| !label.trim().is_empty())
        .ok_or_else(|| ApiError::BadRequest("form question label is invalid".into()))?;
    let mut safe = Map::new();
    safe.insert("type".into(), Value::String(kind.to_owned()));
    safe.insert("label".into(), Value::String(label));
    for (key, max) in [("style", 16), ("description", 100), ("placeholder", 100)] {
        if let Some(value) = safe_optional_text(object, key, max)? {
            if key == "style" && !matches!(value.as_str(), "short" | "paragraph") {
                return Err(ApiError::BadRequest("form text style is invalid".into()));
            }
            safe.insert(key.to_owned(), Value::String(value));
        }
    }
    for key in ["required"] {
        if let Some(value) = optional_bool(object, key)? {
            safe.insert(key.into(), Value::Bool(value));
        }
    }
    for key in ["min_length", "max_length", "min_values", "max_values"] {
        if let Some(value) = object.get(key).filter(|value| !value.is_null()) {
            let number = value
                .as_u64()
                .filter(|number| *number <= 4000)
                .ok_or_else(|| ApiError::BadRequest(format!("form question {key} is invalid")))?;
            safe.insert(key.into(), json!(number));
        }
    }
    if let Some(options) = object.get("options") {
        safe.insert("options".into(), sanitize_form_options(kind, options)?);
    }
    Ok(Value::Object(safe))
}

fn sanitize_form_options(kind: &str, value: &Value) -> Result<Value, ApiError> {
    let max = if kind == "string_select" { 25 } else { 10 };
    let options = value
        .as_array()
        .filter(|options| !options.is_empty() && options.len() <= max)
        .ok_or_else(|| ApiError::BadRequest("form options are invalid".into()))?;
    let mut safe = Vec::with_capacity(options.len());
    let mut values = HashSet::new();
    for option in options {
        let object = only_keys(option, &["label", "value", "description"], "form option")?;
        let label = safe_optional_text(object, "label", 100)?
            .filter(|label| !label.trim().is_empty())
            .ok_or_else(|| ApiError::BadRequest("form option label is invalid".into()))?;
        let value = safe_optional_text(object, "value", 100)?
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ApiError::BadRequest("form option value is invalid".into()))?;
        if !values.insert(value.clone()) {
            return Err(ApiError::BadRequest(
                "form option values must be unique".into(),
            ));
        }
        let description = safe_optional_text(object, "description", 100)?;
        let mut item = json!({ "label": label, "value": value });
        if let Some(description) = description {
            item["description"] = Value::String(description);
        }
        safe.push(item);
    }
    Ok(Value::Array(safe))
}
