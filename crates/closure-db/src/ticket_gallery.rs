use serde_json::Value;
use sqlx::{Postgres, Transaction};

use crate::{Db, DbError};

#[derive(Debug, thiserror::Error)]
pub enum GalleryError {
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("gallery item was not found")]
    NotFound,
    #[error("gallery item changed or the request key was reused")]
    Conflict,
    #[error("gallery cursor does not match the requested sort")]
    InvalidCursor,
}

impl From<sqlx::Error> for GalleryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(DbError::Sqlx(error))
    }
}

impl From<serde_json::Error> for GalleryError {
    fn from(error: serde_json::Error) -> Self {
        Self::Database(DbError::Json(error))
    }
}

#[derive(Debug, Clone)]
pub struct GalleryFilter<'a> {
    pub kind: Option<&'a str>,
    pub category: Option<&'a str>,
    pub query: Option<&'a str>,
    pub sort: &'a str,
    pub offset: i64,
    pub limit: i64,
    /// Opaque keyset position from a previous page; when set, `offset` is ignored.
    pub cursor: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct NewGalleryTemplate {
    pub kind: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub tags: Vec<String>,
    pub payload: Value,
    pub author_name: String,
    pub source_guild_id: i64,
}

#[derive(Debug, Clone)]
pub struct GalleryCard {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub tags: Vec<String>,
    pub author_name: String,
    pub import_count: i64,
    pub created_at: i64,
    pub featured: bool,
}

#[derive(Debug, Clone)]
pub struct GalleryTemplate {
    pub card: GalleryCard,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct GallerySubmission {
    pub id: i64,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub category: String,
    pub tags: Vec<String>,
    pub payload: Value,
    pub author_name: String,
    pub status: String,
    pub review_reason: Option<String>,
    pub featured: bool,
    pub import_count: i64,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub published_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct GalleryPage {
    pub featured: Vec<GalleryCard>,
    pub templates: Vec<GalleryCard>,
    pub total: i64,
    pub offset: i64,
    pub next_offset: Option<i64>,
    pub next_cursor: Option<String>,
}

/// Opaque keyset position of a gallery page. Sort keys are whole seconds, the same values
/// the cards expose as `created_at` and `updated_at`, so a row can rebuild its own cursor.
/// Keyset position just after `card` in the given listing order.
fn page_cursor(sort: &str, card: &GalleryCard) -> String {
    if sort == "popular" {
        GalleryCursor::Popular {
            imports: card.import_count,
            at: card.created_at,
            id: card.id,
        }
    } else {
        GalleryCursor::Newest {
            at: card.created_at,
            id: card.id,
        }
    }
    .encode()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GalleryCursor {
    Newest { at: i64, id: i64 },
    Popular { imports: i64, at: i64, id: i64 },
    Owner { at: i64, id: i64 },
}

impl GalleryCursor {
    #[must_use]
    pub fn encode(self) -> String {
        match self {
            Self::Newest { at, id } => format!("n.{at}.{id}"),
            Self::Popular { imports, at, id } => format!("p.{imports}.{at}.{id}"),
            Self::Owner { at, id } => format!("o.{at}.{id}"),
        }
    }

    #[must_use]
    pub fn decode(raw: &str) -> Option<Self> {
        match raw.split('.').collect::<Vec<_>>().as_slice() {
            ["n", at, id] => Some(Self::Newest {
                at: at.parse().ok()?,
                id: id.parse().ok()?,
            }),
            ["p", imports, at, id] => Some(Self::Popular {
                imports: imports.parse().ok()?,
                at: at.parse().ok()?,
                id: id.parse().ok()?,
            }),
            ["o", at, id] => Some(Self::Owner {
                at: at.parse().ok()?,
                id: id.parse().ok()?,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GallerySubmissionPage {
    pub submissions: Vec<GallerySubmission>,
    pub next_cursor: Option<String>,
}

/// Rows removed by one `ticket_gallery_purge_expired` call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GalleryPurge {
    pub requests: u64,
    pub events: u64,
}

/// Gallery requests and events are kept this long; older rows are purged.
pub const GALLERY_RETENTION_DAYS: i64 = 365;
/// Upper bound of rows deleted per table by one purge call.
pub const GALLERY_PURGE_BATCH: i64 = 500;

type CardRow = (
    i64,
    String,
    String,
    String,
    String,
    Value,
    String,
    i64,
    i64,
    bool,
);
type TemplateRow = (
    i64,
    String,
    String,
    String,
    String,
    Value,
    Value,
    String,
    i64,
    i64,
    bool,
);
type SubmissionRow = (
    i64,
    String,
    String,
    String,
    String,
    Value,
    Value,
    String,
    String,
    Option<String>,
    bool,
    i64,
    i64,
    i64,
    i64,
    Option<i64>,
);
type RequestRow = (String, Vec<u8>, i64);

macro_rules! listed_at {
    () => {
        "extract(epoch FROM COALESCE(published_at, created_at))::bigint"
    };
}

macro_rules! owner_at {
    () => {
        "extract(epoch FROM updated_at)::bigint"
    };
}

macro_rules! card_columns {
    () => {
        "id, kind, title, description, category, tags, author_name, import_count,
         extract(epoch FROM COALESCE(published_at, created_at))::bigint, featured"
    };
}

macro_rules! submission_columns {
    () => {
        "id, kind, title, description, category, tags, payload, author_name, status,
         review_reason, featured, import_count, revision,
         extract(epoch FROM created_at)::bigint, extract(epoch FROM updated_at)::bigint,
         extract(epoch FROM published_at)::bigint"
    };
}

fn card_from_row(
    (id, kind, title, description, category, tags, author_name, import_count, created_at, featured): CardRow,
) -> Result<GalleryCard, GalleryError> {
    Ok(GalleryCard {
        id,
        kind,
        title,
        description,
        category,
        tags: serde_json::from_value(tags)?,
        author_name,
        import_count,
        created_at,
        featured,
    })
}

fn template_from_row(
    (
        id,
        kind,
        title,
        description,
        category,
        tags,
        payload,
        author_name,
        import_count,
        created_at,
        featured,
    ): TemplateRow,
) -> Result<GalleryTemplate, GalleryError> {
    Ok(GalleryTemplate {
        card: card_from_row((
            id,
            kind,
            title,
            description,
            category,
            tags,
            author_name,
            import_count,
            created_at,
            featured,
        ))?,
        payload,
    })
}

fn submission_from_row(
    (
        id,
        kind,
        title,
        description,
        category,
        tags,
        payload,
        author_name,
        status,
        review_reason,
        featured,
        import_count,
        revision,
        created_at,
        updated_at,
        published_at,
    ): SubmissionRow,
) -> Result<GallerySubmission, GalleryError> {
    Ok(GallerySubmission {
        id,
        kind,
        title,
        description,
        category,
        tags: serde_json::from_value(tags)?,
        payload,
        author_name,
        status,
        review_reason,
        featured,
        import_count,
        revision,
        created_at,
        updated_at,
        published_at,
    })
}

async fn request_row(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    request_key: &str,
) -> Result<Option<RequestRow>, GalleryError> {
    Ok(sqlx::query_as(
        "SELECT action, fingerprint, template_id
         FROM ticket_gallery_requests
         WHERE actor_id = $1 AND request_key = $2",
    )
    .bind(actor_id)
    .bind(request_key)
    .fetch_optional(&mut **tx)
    .await?)
}

fn matches_request(
    row: &RequestRow,
    action: &str,
    template_id: Option<i64>,
    fingerprint: &[u8; 32],
) -> bool {
    row.0 == action
        && template_id.is_none_or(|template_id| row.2 == template_id)
        && row.1.as_slice() == fingerprint
}

async fn insert_request(
    tx: &mut Transaction<'_, Postgres>,
    actor_id: i64,
    request_key: &str,
    action: &str,
    fingerprint: &[u8; 32],
    template_id: i64,
) -> Result<(), GalleryError> {
    sqlx::query(
        "INSERT INTO ticket_gallery_requests (actor_id, request_key, action, fingerprint, template_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(actor_id)
    .bind(request_key)
    .bind(action)
    .bind(fingerprint.as_slice())
    .bind(template_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn submission_by_id(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    actor_id: Option<i64>,
) -> Result<Option<GallerySubmission>, GalleryError> {
    let row: Option<SubmissionRow> = if let Some(actor_id) = actor_id {
        sqlx::query_as(concat!(
            "SELECT ",
            submission_columns!(),
            " FROM ticket_gallery_templates WHERE id = $1 AND author_id = $2"
        ))
        .bind(id)
        .bind(actor_id)
        .fetch_optional(&mut **tx)
        .await?
    } else {
        sqlx::query_as(concat!(
            "SELECT ",
            submission_columns!(),
            " FROM ticket_gallery_templates WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
    };
    row.map(submission_from_row).transpose()
}

impl Db {
    async fn featured_cards(&self) -> Result<Vec<GalleryCard>, GalleryError> {
        sqlx::query_as::<_, CardRow>(concat!(
            "SELECT ",
            card_columns!(),
            " FROM ticket_gallery_templates
              WHERE status = 'approved' AND featured
              ORDER BY published_at DESC NULLS LAST, id DESC
              LIMIT 6"
        ))
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(card_from_row)
        .collect::<Result<Vec<_>, _>>()
    }

    pub async fn ticket_gallery_list(
        &self,
        filter: &GalleryFilter<'_>,
    ) -> Result<GalleryPage, GalleryError> {
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*)
             FROM ticket_gallery_templates
             WHERE status = 'approved'
               AND ($1::text IS NULL OR kind = $1)
               AND ($2::text IS NULL OR category = $2)
               AND ($3::text IS NULL OR title ILIKE '%' || $3 || '%' OR description ILIKE '%' || $3 || '%')",
        )
        .bind(filter.kind)
        .bind(filter.category)
        .bind(filter.query)
        .fetch_one(&self.pool)
        .await?;

        let cursor = filter
            .cursor
            .map(|raw| GalleryCursor::decode(raw).ok_or(GalleryError::InvalidCursor))
            .transpose()?;
        let (cursor_imports, cursor_at, cursor_id) = match (cursor, filter.sort) {
            (None, _) => (None, None, None),
            (Some(GalleryCursor::Newest { at, id }), "newest") => (None, Some(at), Some(id)),
            (Some(GalleryCursor::Popular { imports, at, id }), "popular") => {
                (Some(imports), Some(at), Some(id))
            }
            _ => return Err(GalleryError::InvalidCursor),
        };
        let offset = if cursor.is_some() { 0 } else { filter.offset };
        let page_len = usize::try_from(filter.limit).unwrap_or(0);

        let mut rows = sqlx::query_as::<_, CardRow>(concat!(
            "SELECT ",
            card_columns!(),
            " FROM ticket_gallery_templates
              WHERE status = 'approved'
                AND ($1::text IS NULL OR kind = $1)
                AND ($2::text IS NULL OR category = $2)
                AND ($3::text IS NULL OR title ILIKE '%' || $3 || '%' OR description ILIKE '%' || $3 || '%')
                AND ($8::bigint IS NULL OR CASE WHEN $4::text = 'popular'
                    THEN (import_count, ",
            listed_at!(),
            ", id) < ($7::bigint, $8::bigint, $9::bigint)
                    ELSE (",
            listed_at!(),
            ", id) < ($8::bigint, $9::bigint) END)
              ORDER BY CASE WHEN $4::text = 'popular' THEN import_count END DESC NULLS LAST,
                       ",
            listed_at!(),
            " DESC, id DESC
              LIMIT $5 OFFSET $6"
        ))
        .bind(filter.kind)
        .bind(filter.category)
        .bind(filter.query)
        .bind(filter.sort)
        .bind(i64::try_from(page_len.saturating_add(1)).unwrap_or(i64::MAX))
        .bind(offset)
        .bind(cursor_imports)
        .bind(cursor_at)
        .bind(cursor_id)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(card_from_row)
        .collect::<Result<Vec<_>, _>>()?;

        let has_more = rows.len() > page_len;
        rows.truncate(page_len);
        let next_cursor = if has_more {
            rows.last().map(|card| page_cursor(filter.sort, card))
        } else {
            None
        };
        let next_offset = if cursor.is_none() {
            i64::try_from(rows.len())
                .ok()
                .and_then(|page_len| offset.checked_add(page_len))
                .filter(|next_offset| *next_offset < total)
        } else {
            None
        };

        let featured = if filter.kind.is_none()
            && filter.category.is_none()
            && filter.query.is_none()
            && filter.offset == 0
            && cursor.is_none()
        {
            self.featured_cards().await?
        } else {
            Vec::new()
        };
        Ok(GalleryPage {
            featured,
            templates: rows,
            total,
            offset: filter.offset,
            next_offset,
            next_cursor,
        })
    }

    pub async fn ticket_gallery_template(
        &self,
        id: i64,
    ) -> Result<Option<GalleryTemplate>, GalleryError> {
        let row: Option<TemplateRow> = sqlx::query_as(
            "SELECT id, kind, title, description, category, tags, payload, author_name, import_count,
                    extract(epoch FROM COALESCE(published_at, created_at))::bigint, featured
             FROM ticket_gallery_templates
             WHERE id = $1 AND status = 'approved'"
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(template_from_row).transpose()
    }

    pub async fn ticket_gallery_submissions(
        &self,
        actor_id: i64,
        query: Option<&str>,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<GallerySubmissionPage, GalleryError> {
        let cursor = cursor
            .map(|raw| GalleryCursor::decode(raw).ok_or(GalleryError::InvalidCursor))
            .transpose()?;
        let (cursor_at, cursor_id) = match cursor {
            None => (None, None),
            Some(GalleryCursor::Owner { at, id }) => (Some(at), Some(id)),
            Some(_) => return Err(GalleryError::InvalidCursor),
        };
        let page_len = usize::try_from(limit).unwrap_or(0);
        let mut rows: Vec<SubmissionRow> = sqlx::query_as(concat!(
            "SELECT ",
            submission_columns!(),
            " FROM ticket_gallery_templates
              WHERE author_id = $1
                AND ($2::text IS NULL OR title ILIKE '%' || $2 || '%' OR description ILIKE '%' || $2 || '%')
                AND ($3::bigint IS NULL OR (",
            owner_at!(),
            ", id) < ($3::bigint, $4::bigint))
              ORDER BY ",
            owner_at!(),
            " DESC, id DESC
              LIMIT $5"
        ))
        .bind(actor_id)
        .bind(query)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(limit.saturating_add(1))
        .fetch_all(&self.pool)
        .await?;

        let has_more = rows.len() > page_len;
        rows.truncate(page_len);
        let next_cursor = if has_more {
            rows.last().map(|row| {
                GalleryCursor::Owner {
                    at: row.14,
                    id: row.0,
                }
                .encode()
            })
        } else {
            None
        };
        let submissions = rows
            .into_iter()
            .map(submission_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(GallerySubmissionPage {
            submissions,
            next_cursor,
        })
    }

    pub async fn ticket_gallery_submit(
        &self,
        actor_id: i64,
        request_key: &str,
        fingerprint: &[u8; 32],
        template: &NewGalleryTemplate,
    ) -> Result<GallerySubmission, GalleryError> {
        let mut tx = self.pool.begin().await?;
        if let Some(row) = request_row(&mut tx, actor_id, request_key).await? {
            if !matches_request(&row, "submit", None, fingerprint) {
                return Err(GalleryError::Conflict);
            }
            let submission = submission_by_id(&mut tx, row.2, Some(actor_id))
                .await?
                .ok_or(GalleryError::NotFound)?;
            tx.commit().await?;
            return Ok(submission);
        }
        let tags = serde_json::to_value(&template.tags)?;
        let row: SubmissionRow = sqlx::query_as(concat!(
            "INSERT INTO ticket_gallery_templates
             (kind, title, description, category, tags, payload, author_id, source_guild_id, author_name, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'pending')
             RETURNING ",
            submission_columns!()
        ))
        .bind(&template.kind)
        .bind(&template.title)
        .bind(&template.description)
        .bind(&template.category)
        .bind(tags)
        .bind(&template.payload)
        .bind(actor_id)
        .bind(template.source_guild_id)
        .bind(&template.author_name)
        .fetch_one(&mut *tx)
        .await?;
        let submission = submission_from_row(row)?;
        insert_request(
            &mut tx,
            actor_id,
            request_key,
            "submit",
            fingerprint,
            submission.id,
        )
        .await?;
        tx.commit().await?;
        Ok(submission)
    }

    pub async fn ticket_gallery_update(
        &self,
        actor_id: i64,
        id: i64,
        expected_revision: i64,
        request_key: &str,
        fingerprint: &[u8; 32],
        template: &NewGalleryTemplate,
    ) -> Result<GallerySubmission, GalleryError> {
        let mut tx = self.pool.begin().await?;
        if let Some(row) = request_row(&mut tx, actor_id, request_key).await? {
            if !matches_request(&row, "update", Some(id), fingerprint) {
                return Err(GalleryError::Conflict);
            }
            let submission = submission_by_id(&mut tx, row.2, Some(actor_id))
                .await?
                .ok_or(GalleryError::NotFound)?;
            tx.commit().await?;
            return Ok(submission);
        }
        let tags = serde_json::to_value(&template.tags)?;
        let row: Option<SubmissionRow> = sqlx::query_as(concat!(
            "UPDATE ticket_gallery_templates
             SET kind = $1, title = $2, description = $3, category = $4, tags = $5, payload = $6,
                 author_name = $7, source_guild_id = $8, status = 'pending', review_reason = NULL, featured = FALSE,
                 published_at = NULL, revision = revision + 1, updated_at = now()
             WHERE id = $9 AND author_id = $10 AND revision = $11
               AND status NOT IN ('withdrawn', 'unpublished')
             RETURNING ",
            submission_columns!()
        ))
        .bind(&template.kind)
        .bind(&template.title)
        .bind(&template.description)
        .bind(&template.category)
        .bind(tags)
        .bind(&template.payload)
        .bind(&template.author_name)
        .bind(template.source_guild_id)
        .bind(id)
        .bind(actor_id)
        .bind(expected_revision)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            let owned = submission_by_id(&mut tx, id, Some(actor_id)).await?;
            return Err(if owned.is_some() {
                GalleryError::Conflict
            } else {
                GalleryError::NotFound
            });
        };
        let submission = submission_from_row(row)?;
        insert_request(&mut tx, actor_id, request_key, "update", fingerprint, id).await?;
        tx.commit().await?;
        Ok(submission)
    }

    pub async fn ticket_gallery_change_submission(
        &self,
        actor_id: i64,
        id: i64,
        expected_revision: i64,
        request_key: &str,
        fingerprint: &[u8; 32],
        action: &str,
    ) -> Result<GallerySubmission, GalleryError> {
        let (from_status, to_status) = match action {
            "withdraw" => ("pending", "withdrawn"),
            "unpublish" => ("approved", "unpublished"),
            _ => return Err(GalleryError::Conflict),
        };
        let mut tx = self.pool.begin().await?;
        if let Some(row) = request_row(&mut tx, actor_id, request_key).await? {
            if !matches_request(&row, action, Some(id), fingerprint) {
                return Err(GalleryError::Conflict);
            }
            let submission = submission_by_id(&mut tx, row.2, Some(actor_id))
                .await?
                .ok_or(GalleryError::NotFound)?;
            tx.commit().await?;
            return Ok(submission);
        }
        let row: Option<SubmissionRow> = sqlx::query_as(concat!(
            "UPDATE ticket_gallery_templates
             SET status = $1, revision = revision + 1, updated_at = now(),
                 featured = FALSE, published_at = NULL
             WHERE id = $2 AND author_id = $3 AND revision = $4 AND status = $5
             RETURNING ",
            submission_columns!()
        ))
        .bind(to_status)
        .bind(id)
        .bind(actor_id)
        .bind(expected_revision)
        .bind(from_status)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            let owned = submission_by_id(&mut tx, id, Some(actor_id)).await?;
            return Err(if owned.is_some() {
                GalleryError::Conflict
            } else {
                GalleryError::NotFound
            });
        };
        let submission = submission_from_row(row)?;
        insert_request(&mut tx, actor_id, request_key, action, fingerprint, id).await?;
        tx.commit().await?;
        Ok(submission)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn ticket_gallery_review(
        &self,
        reviewer_id: i64,
        id: i64,
        expected_revision: i64,
        request_key: &str,
        fingerprint: &[u8; 32],
        status: &str,
        reason: Option<&str>,
        featured: bool,
    ) -> Result<GallerySubmission, GalleryError> {
        let action = match status {
            "approved" => "review_approved",
            "rejected" => "review_rejected",
            _ => return Err(GalleryError::Conflict),
        };
        let mut tx = self.pool.begin().await?;
        if let Some(row) = request_row(&mut tx, reviewer_id, request_key).await? {
            if !matches_request(&row, action, Some(id), fingerprint) {
                return Err(GalleryError::Conflict);
            }
            let submission = submission_by_id(&mut tx, row.2, None)
                .await?
                .ok_or(GalleryError::NotFound)?;
            tx.commit().await?;
            return Ok(submission);
        }
        let row: Option<SubmissionRow> = sqlx::query_as(concat!(
            "UPDATE ticket_gallery_templates
             SET status = $1, review_reason = $2, featured = $3, revision = revision + 1,
                 published_at = CASE WHEN $1 = 'approved' THEN now() ELSE NULL END,
                 updated_at = now()
             WHERE id = $4 AND revision = $5 AND status = 'pending'
             RETURNING ",
            submission_columns!()
        ))
        .bind(status)
        .bind(reason)
        .bind(featured && status == "approved")
        .bind(id)
        .bind(expected_revision)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            let existing = submission_by_id(&mut tx, id, None).await?;
            return Err(if existing.is_some() {
                GalleryError::Conflict
            } else {
                GalleryError::NotFound
            });
        };
        let submission = submission_from_row(row)?;
        insert_request(&mut tx, reviewer_id, request_key, action, fingerprint, id).await?;
        let event_type = if status == "approved" {
            "gallery.approved"
        } else {
            "gallery.rejected"
        };
        let event_id: i64 = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_events
             (submission_id, guild_id, revision, recipient_id, reviewer_id, event_type, status, reason, title)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING event_id",
        )
        .bind(id)
        .bind(submission_source_guild_id(&mut tx, id).await?)
        .bind(submission.revision)
        .bind(submission_author_id(&mut tx, id).await?)
        .bind(reviewer_id)
        .bind(event_type)
        .bind(status)
        .bind(reason)
        .bind(&submission.title)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO ticket_notification_outbox (event_id) VALUES ($1)")
            .bind(event_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(submission)
    }

    /// Records a successful import by a guild. `import_count` moves only when this guild
    /// has not imported the template before, so repeated requests cannot inflate it.
    pub async fn ticket_gallery_record_import(
        &self,
        actor_id: i64,
        guild_id: i64,
        id: i64,
        request_key: &str,
        fingerprint: &[u8; 32],
    ) -> Result<i64, GalleryError> {
        let mut tx = self.pool.begin().await?;
        if let Some(row) = request_row(&mut tx, actor_id, request_key).await? {
            if !matches_request(&row, "import", Some(id), fingerprint) {
                return Err(GalleryError::Conflict);
            }
            let count: Option<i64> = sqlx::query_scalar(
                "SELECT import_count FROM ticket_gallery_templates WHERE id = $1",
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
            let count = count.ok_or(GalleryError::NotFound)?;
            tx.commit().await?;
            return Ok(count);
        }
        // Locking the approved row serializes concurrent imports of one template.
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT import_count FROM ticket_gallery_templates
             WHERE id = $1 AND status = 'approved'
             FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let current = current.ok_or(GalleryError::NotFound)?;
        let recorded: Option<i64> = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_installations (guild_id, template_id)
             VALUES ($1, $2)
             ON CONFLICT (guild_id, template_id) DO NOTHING
             RETURNING template_id",
        )
        .bind(guild_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let count = if recorded.is_some() {
            sqlx::query_scalar(
                "UPDATE ticket_gallery_templates
                 SET import_count = import_count + 1
                 WHERE id = $1
                 RETURNING import_count",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await?
        } else {
            current
        };
        insert_request(&mut tx, actor_id, request_key, "import", fingerprint, id).await?;
        tx.commit().await?;
        Ok(count)
    }

    /// Deletes up to `GALLERY_PURGE_BATCH` request and event rows per table that are older
    /// than `GALLERY_RETENTION_DAYS`. Events with a pending notification are kept.
    pub async fn ticket_gallery_purge_expired(&self) -> Result<GalleryPurge, GalleryError> {
        let requests = sqlx::query(
            "DELETE FROM ticket_gallery_requests
             WHERE (actor_id, request_key) IN (
                 SELECT actor_id, request_key
                 FROM ticket_gallery_requests
                 WHERE created_at < now() - make_interval(days => $1::int)
                 ORDER BY created_at
                 LIMIT $2
             )",
        )
        .bind(GALLERY_RETENTION_DAYS)
        .bind(GALLERY_PURGE_BATCH)
        .execute(&self.pool)
        .await?
        .rows_affected();
        let events = sqlx::query(
            "DELETE FROM ticket_gallery_events
             WHERE event_id IN (
                 SELECT e.event_id
                 FROM ticket_gallery_events e
                 WHERE e.occurred_at < now() - make_interval(days => $1::int)
                   AND NOT EXISTS (
                       SELECT 1 FROM ticket_notification_outbox o
                       WHERE o.event_id = e.event_id
                         AND o.completed_at IS NULL AND o.abandoned_at IS NULL
                   )
                 ORDER BY e.occurred_at
                 LIMIT $2
             )",
        )
        .bind(GALLERY_RETENTION_DAYS)
        .bind(GALLERY_PURGE_BATCH)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(GalleryPurge { requests, events })
    }
}

async fn submission_source_guild_id(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<i64, GalleryError> {
    sqlx::query_scalar("SELECT source_guild_id FROM ticket_gallery_templates WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(GalleryError::NotFound)
}

async fn submission_author_id(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
) -> Result<i64, GalleryError> {
    sqlx::query_scalar("SELECT author_id FROM ticket_gallery_templates WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(GalleryError::NotFound)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    const FINGERPRINT: [u8; 32] = [1_u8; 32];

    async fn approved_template(pool: &PgPool, author_id: i64, minutes_ago: i32) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO ticket_gallery_templates
             (kind, title, description, category, payload, author_id, source_guild_id, author_name, status, published_at)
             VALUES ('panel', 'Title', 'Description', 'general', '{}', $1, 900, 'Author', 'approved',
                     now() - make_interval(mins => $2))
             RETURNING id",
        )
        .bind(author_id)
        .bind(minutes_ago)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    fn newest_filter(offset: i64, cursor: Option<&str>) -> GalleryFilter<'_> {
        GalleryFilter {
            kind: None,
            category: None,
            query: None,
            sort: "newest",
            offset,
            limit: 2,
            cursor,
        }
    }

    #[sqlx::test(migrations = false)]
    async fn list_pages_do_not_repeat_rows_when_a_newer_template_appears(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        for minutes_ago in 1..=4 {
            approved_template(&pool, 9, minutes_ago).await;
        }
        let first = db
            .ticket_gallery_list(&newest_filter(0, None))
            .await
            .unwrap();
        let first_ids: Vec<i64> = first.templates.iter().map(|card| card.id).collect();
        approved_template(&pool, 9, 0).await;

        let cursor = first
            .next_cursor
            .as_deref()
            .expect("the first page has a next cursor");
        let second = db
            .ticket_gallery_list(&newest_filter(0, Some(cursor)))
            .await
            .unwrap();
        let second_ids: Vec<i64> = second.templates.iter().map(|card| card.id).collect();

        let mut all = first_ids.clone();
        all.extend(second_ids);
        let mut unique = all.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(all.len(), 4);
        assert_eq!(unique.len(), all.len(), "a row was returned twice: {all:?}");
    }

    #[test]
    fn cursors_round_trip_and_reject_other_shapes() {
        let cursors = [
            GalleryCursor::Newest { at: -5, id: 7 },
            GalleryCursor::Popular {
                imports: 3,
                at: 1_700_000_000,
                id: 9,
            },
            GalleryCursor::Owner {
                at: 1_700_000_001,
                id: 11,
            },
        ];
        for cursor in cursors {
            assert_eq!(GalleryCursor::decode(&cursor.encode()), Some(cursor));
        }
        for raw in ["", "x.1.2", "n.1", "p.1.2", "n.a.2", "o.1.2.3"] {
            assert_eq!(GalleryCursor::decode(raw), None, "{raw} must be rejected");
        }
    }

    #[sqlx::test(migrations = false)]
    async fn an_owner_sees_every_submission_across_pages(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        sqlx::query(
            "INSERT INTO ticket_gallery_templates
             (kind, title, description, category, payload, author_id, source_guild_id, author_name)
             SELECT 'panel', 'Title', 'Description', 'general', '{}', 7, 900, 'Author'
             FROM generate_series(1, 105)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = db
                .ticket_gallery_submissions(7, None, cursor.as_deref(), 50)
                .await
                .unwrap();
            seen.extend(page.submissions.iter().map(|submission| submission.id));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(seen.len(), 105);
        assert_eq!(unique.len(), 105);
    }

    #[sqlx::test(migrations = false)]
    async fn a_guild_counts_once_however_many_requests_it_sends(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        let id = approved_template(&pool, 9, 1).await;

        let first = db
            .ticket_gallery_record_import(7, 800, id, "import-a", &FINGERPRINT)
            .await
            .unwrap();
        let second = db
            .ticket_gallery_record_import(8, 800, id, "import-b", &FINGERPRINT)
            .await
            .unwrap();
        let replayed = db
            .ticket_gallery_record_import(7, 800, id, "import-a", &FINGERPRINT)
            .await
            .unwrap();

        assert_eq!(first, 1);
        assert_eq!(second, 1);
        assert_eq!(replayed, 1);
    }

    #[sqlx::test(migrations = false)]
    async fn each_distinct_guild_adds_one_import(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        let id = approved_template(&pool, 9, 1).await;

        db.ticket_gallery_record_import(7, 800, id, "import-a", &FINGERPRINT)
            .await
            .unwrap();
        let count = db
            .ticket_gallery_record_import(7, 801, id, "import-b", &FINGERPRINT)
            .await
            .unwrap();

        assert_eq!(count, 2);
    }

    #[sqlx::test(migrations = false)]
    async fn purge_removes_old_rows_in_bounded_batches(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        let id = approved_template(&pool, 9, 1).await;
        sqlx::query(
            "INSERT INTO ticket_gallery_requests (actor_id, request_key, action, fingerprint, template_id, created_at)
             SELECT 7, 'old-' || n, 'import', $1, $2, now() - make_interval(days => 400)
             FROM generate_series(1, 600) AS n",
        )
        .bind(FINGERPRINT.as_slice())
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ticket_gallery_requests (actor_id, request_key, action, fingerprint, template_id)
             VALUES (7, 'recent', 'import', $1, $2)",
        )
        .bind(FINGERPRINT.as_slice())
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let first = db.ticket_gallery_purge_expired().await.unwrap();
        let second = db.ticket_gallery_purge_expired().await.unwrap();

        assert_eq!(first.requests, 500);
        assert_eq!(second.requests, 100);
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM ticket_gallery_requests")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[sqlx::test(migrations = false)]
    async fn purge_keeps_old_events_that_still_await_delivery(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        let id = approved_template(&pool, 9, 1).await;
        let old_delivered: i64 = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_events
             (submission_id, guild_id, revision, recipient_id, reviewer_id, event_type, status, title, occurred_at)
             VALUES ($1, 900, 1, 9, 5, 'gallery.approved', 'approved', 'Title', now() - make_interval(days => 400))
             RETURNING event_id",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let old_pending: i64 = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_events
             (submission_id, guild_id, revision, recipient_id, reviewer_id, event_type, status, title, occurred_at)
             VALUES ($1, 900, 2, 9, 5, 'gallery.approved', 'approved', 'Title', now() - make_interval(days => 400))
             RETURNING event_id",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ticket_notification_outbox (event_id, completed_at) VALUES ($1, now())",
        )
        .bind(old_delivered)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO ticket_notification_outbox (event_id) VALUES ($1)")
            .bind(old_pending)
            .execute(&pool)
            .await
            .unwrap();

        let purged = db.ticket_gallery_purge_expired().await.unwrap();

        assert_eq!(purged.events, 1);
        let remaining: Vec<i64> = sqlx::query_scalar("SELECT event_id FROM ticket_gallery_events")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, [old_pending]);
    }
}
