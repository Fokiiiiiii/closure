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
}

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

        let templates = sqlx::query_as::<_, CardRow>(concat!(
            "SELECT ",
            card_columns!(),
            " FROM ticket_gallery_templates
             WHERE status = 'approved'
               AND ($1::text IS NULL OR kind = $1)
               AND ($2::text IS NULL OR category = $2)
               AND ($3::text IS NULL OR title ILIKE '%' || $3 || '%' OR description ILIKE '%' || $3 || '%')
             ORDER BY CASE WHEN $4::text = 'popular' THEN import_count END DESC NULLS LAST,
                      published_at DESC NULLS LAST, id DESC
             LIMIT $5 OFFSET $6"
        ))
        .bind(filter.kind)
        .bind(filter.category)
        .bind(filter.query)
        .bind(filter.sort)
        .bind(filter.limit)
        .bind(filter.offset)
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(card_from_row)
        .collect::<Result<Vec<_>, _>>()?;

        let featured = if filter.kind.is_none()
            && filter.category.is_none()
            && filter.query.is_none()
            && filter.offset == 0
        {
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
            .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        let next_offset = (filter.offset + (templates.len() as i64) < total)
            .then_some(filter.offset + templates.len() as i64);
        Ok(GalleryPage {
            featured,
            templates,
            total,
            offset: filter.offset,
            next_offset,
        })
    }

    pub async fn ticket_gallery_template(
        &self,
        id: i64,
    ) -> Result<Option<GalleryTemplate>, GalleryError> {
        let row: Option<TemplateRow> = sqlx::query_as(concat!(
            "SELECT id, kind, title, description, category, tags, payload, author_name, import_count,
                    extract(epoch FROM COALESCE(published_at, created_at))::bigint, featured
             FROM ticket_gallery_templates
             WHERE id = $1 AND status = 'approved'"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(template_from_row).transpose()
    }

    pub async fn ticket_gallery_submissions(
        &self,
        actor_id: i64,
    ) -> Result<Vec<GallerySubmission>, GalleryError> {
        let rows: Vec<SubmissionRow> = sqlx::query_as(concat!(
            "SELECT ",
            submission_columns!(),
            " FROM ticket_gallery_templates
             WHERE author_id = $1
             ORDER BY updated_at DESC, id DESC
             LIMIT 100"
        ))
        .bind(actor_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(submission_from_row)
            .collect::<Result<Vec<_>, _>>()
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

    pub async fn ticket_gallery_record_import(
        &self,
        actor_id: i64,
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
        let count: Option<i64> = sqlx::query_scalar(
            "UPDATE ticket_gallery_templates
             SET import_count = import_count + 1
             WHERE id = $1 AND status = 'approved'
             RETURNING import_count",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let count = count.ok_or(GalleryError::NotFound)?;
        insert_request(&mut tx, actor_id, request_key, "import", fingerprint, id).await?;
        tx.commit().await?;
        Ok(count)
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
