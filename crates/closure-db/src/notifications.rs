use sqlx::{FromRow, Postgres, Transaction};
use subtle::ConstantTimeEq;

use crate::{Db, DbError};

const PREFERENCES_SELECT: &str = "
    SELECT user_id,
           in_app_enabled,
           discord_dm_enabled,
           email_enabled,
           revision,
           locale,
           email_ciphertext,
           email_masked,
           pending_email_ciphertext,
           pending_email_masked,
           email_verified,
           email_code_hash,
           EXTRACT(EPOCH FROM email_code_expires_at)::BIGINT AS email_code_expires_at,
           email_code_attempts,
           EXTRACT(EPOCH FROM email_requested_at)::BIGINT AS email_requested_at,
           last_email_request_key,
           last_email_request_fingerprint
    FROM ticket_notification_preferences
    WHERE user_id = $1";

#[derive(sqlx::FromRow)]
struct PreferencesRow {
    user_id: i64,
    in_app_enabled: bool,
    discord_dm_enabled: bool,
    email_enabled: bool,
    revision: i64,
    locale: String,
    email_ciphertext: Option<Vec<u8>>,
    email_masked: Option<String>,
    pending_email_ciphertext: Option<Vec<u8>>,
    pending_email_masked: Option<String>,
    email_verified: bool,
    email_code_hash: Option<Vec<u8>>,
    email_code_expires_at: Option<i64>,
    email_code_attempts: i16,
    email_requested_at: Option<i64>,
    last_email_request_key: Option<String>,
    last_email_request_fingerprint: Option<Vec<u8>>,
}

impl From<PreferencesRow> for StoredNotificationPreferences {
    fn from(row: PreferencesRow) -> Self {
        Self {
            user_id: row.user_id,
            in_app_enabled: row.in_app_enabled,
            discord_dm_enabled: row.discord_dm_enabled,
            email_enabled: row.email_enabled,
            revision: row.revision,
            locale: row.locale,
            email_ciphertext: row.email_ciphertext,
            email_masked: row.email_masked,
            pending_email_ciphertext: row.pending_email_ciphertext,
            pending_email_masked: row.pending_email_masked,
            email_verified: row.email_verified,
            email_code_hash: row.email_code_hash,
            email_code_expires_at: row.email_code_expires_at,
            email_code_attempts: row.email_code_attempts,
            email_requested_at: row.email_requested_at,
            last_email_request_key: row.last_email_request_key,
            last_email_request_fingerprint: row.last_email_request_fingerprint,
        }
    }
}

#[derive(Clone)]
pub struct StoredNotificationPreferences {
    pub user_id: i64,
    pub in_app_enabled: bool,
    pub discord_dm_enabled: bool,
    pub email_enabled: bool,
    pub revision: i64,
    pub locale: String,
    pub email_ciphertext: Option<Vec<u8>>,
    pub email_masked: Option<String>,
    pub pending_email_ciphertext: Option<Vec<u8>>,
    pub pending_email_masked: Option<String>,
    pub email_verified: bool,
    pub email_code_hash: Option<Vec<u8>>,
    pub email_code_expires_at: Option<i64>,
    pub email_code_attempts: i16,
    pub email_requested_at: Option<i64>,
    pub last_email_request_key: Option<String>,
    pub last_email_request_fingerprint: Option<Vec<u8>>,
}

impl Default for StoredNotificationPreferences {
    fn default() -> Self {
        Self {
            user_id: 0,
            in_app_enabled: true,
            discord_dm_enabled: false,
            email_enabled: false,
            revision: 0,
            locale: "en".to_owned(),
            email_ciphertext: None,
            email_masked: None,
            pending_email_ciphertext: None,
            pending_email_masked: None,
            email_verified: false,
            email_code_hash: None,
            email_code_expires_at: None,
            email_code_attempts: 0,
            email_requested_at: None,
            last_email_request_key: None,
            last_email_request_fingerprint: None,
        }
    }
}

pub struct NotificationItem {
    pub id: i64,
    pub source_event_id: i64,
    pub source_guild_id: i64,
    pub category: String,
    pub event_type: String,
    pub submission_title: String,
    pub reason: Option<String>,
    pub href: String,
    pub created_at: i64,
    pub created_at_iso: String,
    pub read_at: Option<i64>,
}

#[derive(FromRow)]
struct NotificationRow {
    id: i64,
    source_event_id: i64,
    source_guild_id: i64,
    category: String,
    event_type: String,
    submission_title: String,
    reason: Option<String>,
    href: String,
    created_at: i64,
    created_at_iso: String,
    read_at: Option<i64>,
}

impl From<NotificationRow> for NotificationItem {
    fn from(row: NotificationRow) -> Self {
        Self {
            id: row.id,
            source_event_id: row.source_event_id,
            source_guild_id: row.source_guild_id,
            category: row.category,
            event_type: row.event_type,
            submission_title: row.submission_title,
            reason: row.reason,
            href: row.href,
            created_at: row.created_at,
            created_at_iso: row.created_at_iso,
            read_at: row.read_at,
        }
    }
}

pub struct NotificationPage {
    pub preferences: StoredNotificationPreferences,
    pub notifications: Vec<NotificationItem>,
    pub unread_count: i64,
    pub next_cursor: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub enum EmailRequestOutcome {
    Started { revision: i64 },
    Replay { revision: i64 },
    Cooldown { retry_after_seconds: u64 },
    IdempotencyConflict,
}

#[derive(Debug, Clone, Copy)]
pub enum EmailVerificationOutcome {
    Verified { revision: i64 },
    AlreadyVerified { revision: i64 },
    Invalid { revision: i64 },
    Expired { revision: i64 },
}

#[derive(Debug, Clone, Copy)]
pub enum NotificationOutboxChannel {
    InApp,
    DiscordDm,
    Email,
}

#[derive(FromRow)]
struct OutboxRow {
    event_id: i64,
    attempts: i32,
    in_app_done: bool,
    discord_dm_done: bool,
    email_done: bool,
    guild_id: i64,
    recipient_id: i64,
    submission_id: i64,
    event_type: String,
    status: String,
    reason: Option<String>,
    title: String,
    occurred_at: i64,
}

pub struct ClaimedNotificationEvent {
    pub event_id: i64,
    pub attempts: i32,
    pub guild_id: u64,
    pub recipient_id: u64,
    pub submission_id: i64,
    pub event_type: String,
    pub status: String,
    pub reason: Option<String>,
    pub title: String,
    pub occurred_at: i64,
    pub in_app_done: bool,
    pub discord_dm_done: bool,
    pub email_done: bool,
}

impl TryFrom<OutboxRow> for ClaimedNotificationEvent {
    type Error = DbError;

    fn try_from(row: OutboxRow) -> Result<Self, Self::Error> {
        Ok(Self {
            event_id: row.event_id,
            attempts: row.attempts,
            guild_id: u64::try_from(row.guild_id)
                .map_err(|_| DbError::InvalidStoredId(row.guild_id))?,
            recipient_id: u64::try_from(row.recipient_id)
                .map_err(|_| DbError::InvalidStoredId(row.recipient_id))?,
            submission_id: row.submission_id,
            event_type: row.event_type,
            status: row.status,
            reason: row.reason,
            title: row.title,
            occurred_at: row.occurred_at,
            in_app_done: row.in_app_done,
            discord_dm_done: row.discord_dm_done,
            email_done: row.email_done,
        })
    }
}

fn snowflake(raw: u64) -> Result<i64, DbError> {
    i64::try_from(raw)
        .ok()
        .filter(|id| *id > 0)
        .ok_or(DbError::IdOutOfRange(raw))
}

async fn ensure_preferences(
    tx: &mut Transaction<'_, Postgres>,
    user_id: i64,
) -> Result<StoredNotificationPreferences, DbError> {
    sqlx::query(
        "INSERT INTO ticket_notification_preferences (user_id) VALUES ($1) ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .execute(&mut **tx)
    .await?;

    let sql = format!("{PREFERENCES_SELECT} FOR UPDATE");
    let row = sqlx::query_as::<_, PreferencesRow>(&sql)
        .bind(user_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(row.into())
}

async fn bump_revision(tx: &mut Transaction<'_, Postgres>, user_id: i64) -> Result<i64, DbError> {
    Ok(sqlx::query_scalar(
        "UPDATE ticket_notification_preferences
         SET revision = revision + 1, updated_at = now()
         WHERE user_id = $1
         RETURNING revision",
    )
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?)
}

impl Db {
    pub async fn notification_preferences(
        &self,
        user: u64,
    ) -> Result<StoredNotificationPreferences, DbError> {
        let user = snowflake(user)?;
        let row = sqlx::query_as::<_, PreferencesRow>(PREFERENCES_SELECT)
            .bind(user)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(Into::into).unwrap_or_default())
    }

    pub async fn set_notification_locale(&self, user: u64, locale: &str) -> Result<i64, DbError> {
        if !matches!(locale, "en" | "ja") {
            return Err(DbError::Invalid("invalid notification locale".into()));
        }
        let user = snowflake(user)?;
        let mut tx = self.pool.begin().await?;
        let current = ensure_preferences(&mut tx, user).await?;
        if current.locale == locale {
            tx.commit().await?;
            return Ok(current.revision);
        }
        let revision = sqlx::query_scalar::<_, i64>(
            "UPDATE ticket_notification_preferences
             SET locale = $2, revision = revision + 1, updated_at = now()
             WHERE user_id = $1
             RETURNING revision",
        )
        .bind(user)
        .bind(locale)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(revision)
    }

    pub async fn notification_page(
        &self,
        user: u64,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<NotificationPage, DbError> {
        let user = snowflake(user)?;
        let limit = limit.clamp(1, 100);
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;

        let preferences = sqlx::query_as::<_, PreferencesRow>(PREFERENCES_SELECT)
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?
            .map(Into::into)
            .unwrap_or_default();
        let unread_count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM ticket_notifications WHERE user_id = $1 AND read_at IS NULL",
        )
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        let mut rows = sqlx::query_as::<_, NotificationRow>(
            "SELECT id, source_event_id, source_guild_id, category, event_type,
                    submission_title, reason, href,
                    EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at,
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z') AS created_at_iso,
                    EXTRACT(EPOCH FROM read_at)::BIGINT AS read_at
             FROM ticket_notifications
             WHERE user_id = $1 AND ($2::BIGINT IS NULL OR id < $2)
             ORDER BY id DESC
             LIMIT $3",
        )
        .bind(user)
        .bind(cursor)
        .bind(i64::try_from(limit + 1).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = has_more.then(|| rows.last().map(|row| row.id)).flatten();
        let notifications = rows.into_iter().map(Into::into).collect();
        tx.commit().await?;
        Ok(NotificationPage {
            preferences,
            notifications,
            unread_count,
            next_cursor,
        })
    }

    pub async fn save_notification_preferences(
        &self,
        user: u64,
        expected_revision: i64,
        in_app_enabled: bool,
        discord_dm_enabled: bool,
        email_enabled: bool,
    ) -> Result<i64, DbError> {
        let user = snowflake(user)?;
        let mut tx = self.pool.begin().await?;
        let current = ensure_preferences(&mut tx, user).await?;
        if (
            current.in_app_enabled,
            current.discord_dm_enabled,
            current.email_enabled,
        ) == (in_app_enabled, discord_dm_enabled, email_enabled)
        {
            tx.commit().await?;
            return Ok(current.revision);
        }
        if current.revision != expected_revision {
            return Err(DbError::RevisionConflict {
                expected: Some(expected_revision),
                actual: Some(current.revision),
            });
        }
        if email_enabled && (!current.email_verified || current.email_ciphertext.is_none()) {
            return Err(DbError::Invalid(
                "email notifications need a verified address".into(),
            ));
        }
        let revision = sqlx::query_scalar::<_, i64>(
            "UPDATE ticket_notification_preferences
             SET in_app_enabled = $2, discord_dm_enabled = $3, email_enabled = $4,
                 revision = revision + 1, updated_at = now()
             WHERE user_id = $1
             RETURNING revision",
        )
        .bind(user)
        .bind(in_app_enabled)
        .bind(discord_dm_enabled)
        .bind(email_enabled)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(revision)
    }

    pub async fn mark_notification_read(
        &self,
        user: u64,
        expected_revision: i64,
        notification_id: i64,
    ) -> Result<Option<i64>, DbError> {
        let user = snowflake(user)?;
        if notification_id <= 0 {
            return Ok(None);
        }
        let mut tx = self.pool.begin().await?;
        let current = ensure_preferences(&mut tx, user).await?;
        let read_at = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT EXTRACT(EPOCH FROM read_at)::BIGINT
             FROM ticket_notifications WHERE user_id = $1 AND id = $2",
        )
        .bind(user)
        .bind(notification_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(read_at) = read_at else {
            tx.commit().await?;
            return Ok(None);
        };
        if read_at.is_some() {
            tx.commit().await?;
            return Ok(Some(current.revision));
        }
        if current.revision != expected_revision {
            return Err(DbError::RevisionConflict {
                expected: Some(expected_revision),
                actual: Some(current.revision),
            });
        }
        sqlx::query(
            "UPDATE ticket_notifications SET read_at = now()
             WHERE user_id = $1 AND id = $2 AND read_at IS NULL",
        )
        .bind(user)
        .bind(notification_id)
        .execute(&mut *tx)
        .await?;
        let revision = bump_revision(&mut tx, user).await?;
        tx.commit().await?;
        Ok(Some(revision))
    }

    pub async fn mark_all_notifications_read(
        &self,
        user: u64,
        expected_revision: i64,
    ) -> Result<i64, DbError> {
        let user = snowflake(user)?;
        let mut tx = self.pool.begin().await?;
        let current = ensure_preferences(&mut tx, user).await?;
        let unread: bool = sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM ticket_notifications WHERE user_id = $1 AND read_at IS NULL
             )",
        )
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        if !unread {
            tx.commit().await?;
            return Ok(current.revision);
        }
        if current.revision != expected_revision {
            return Err(DbError::RevisionConflict {
                expected: Some(expected_revision),
                actual: Some(current.revision),
            });
        }
        sqlx::query(
            "UPDATE ticket_notifications SET read_at = now()
             WHERE user_id = $1 AND read_at IS NULL",
        )
        .bind(user)
        .execute(&mut *tx)
        .await?;
        let revision = bump_revision(&mut tx, user).await?;
        tx.commit().await?;
        Ok(revision)
    }

    pub async fn begin_email_verification(
        &self,
        user: u64,
        expected_revision: i64,
        email_ciphertext: Vec<u8>,
        email_masked: String,
        code_hash: Vec<u8>,
        code_expires_at: i64,
        request_key: String,
        request_fingerprint: Vec<u8>,
        now: i64,
    ) -> Result<EmailRequestOutcome, DbError> {
        let user = snowflake(user)?;
        if code_hash.len() != 32 || request_fingerprint.len() != 32 {
            return Err(DbError::Invalid("invalid email verification state".into()));
        }
        let mut tx = self.pool.begin().await?;
        let current = ensure_preferences(&mut tx, user).await?;

        if current.last_email_request_key.as_deref() == Some(request_key.as_str()) {
            let matches = current
                .last_email_request_fingerprint
                .as_deref()
                .is_some_and(|stored| bool::from(stored.ct_eq(&request_fingerprint)));
            if !matches {
                return Ok(EmailRequestOutcome::IdempotencyConflict);
            }
            if current.email_verified
                || current
                    .email_code_expires_at
                    .is_some_and(|expires| expires > now)
            {
                tx.commit().await?;
                return Ok(EmailRequestOutcome::Replay {
                    revision: current.revision,
                });
            }
        }

        if let Some(requested_at) = current.email_requested_at {
            let elapsed = now.saturating_sub(requested_at).clamp(0, 60);
            if elapsed < 60 {
                tx.commit().await?;
                return Ok(EmailRequestOutcome::Cooldown {
                    retry_after_seconds: u64::try_from(60 - elapsed).unwrap_or(60),
                });
            }
        }
        if current.revision != expected_revision {
            return Err(DbError::RevisionConflict {
                expected: Some(expected_revision),
                actual: Some(current.revision),
            });
        }

        let revision = sqlx::query_scalar::<_, i64>(
            "UPDATE ticket_notification_preferences
             SET pending_email_ciphertext = $2,
                 pending_email_masked = $3,
                 email_enabled = FALSE,
                 email_verified = FALSE,
                 email_code_hash = $4,
                 email_code_expires_at = to_timestamp($5),
                 email_code_attempts = 0,
                 email_requested_at = to_timestamp($6),
                 last_email_request_key = $7,
                 last_email_request_fingerprint = $8,
                 revision = revision + 1,
                 updated_at = now()
             WHERE user_id = $1
             RETURNING revision",
        )
        .bind(user)
        .bind(email_ciphertext)
        .bind(email_masked)
        .bind(code_hash)
        .bind(code_expires_at)
        .bind(now)
        .bind(request_key)
        .bind(request_fingerprint)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(EmailRequestOutcome::Started { revision })
    }

    pub async fn cancel_email_verification(
        &self,
        user: u64,
        request_key: &str,
    ) -> Result<(), DbError> {
        let user = snowflake(user)?;
        sqlx::query(
            "UPDATE ticket_notification_preferences
             SET pending_email_ciphertext = NULL,
                 pending_email_masked = NULL,
                 email_code_hash = NULL,
                 email_code_expires_at = NULL,
                 email_code_attempts = 0,
                 email_requested_at = NULL,
                 last_email_request_key = NULL,
                 last_email_request_fingerprint = NULL,
                 revision = revision + 1,
                 updated_at = now()
             WHERE user_id = $1 AND last_email_request_key = $2",
        )
        .bind(user)
        .bind(request_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn verify_notification_email(
        &self,
        user: u64,
        expected_revision: i64,
        candidate_hash: &[u8],
        now: i64,
    ) -> Result<EmailVerificationOutcome, DbError> {
        let user = snowflake(user)?;
        if candidate_hash.len() != 32 {
            return Err(DbError::Invalid("invalid verification code".into()));
        }
        let mut tx = self.pool.begin().await?;
        let current = ensure_preferences(&mut tx, user).await?;
        if current.email_verified && current.pending_email_ciphertext.is_none() {
            tx.commit().await?;
            return Ok(EmailVerificationOutcome::AlreadyVerified {
                revision: current.revision,
            });
        }
        let Some(stored_hash) = current.email_code_hash.as_deref() else {
            tx.commit().await?;
            return Ok(EmailVerificationOutcome::Expired {
                revision: current.revision,
            });
        };
        let expires_at = current.email_code_expires_at.unwrap_or(0);
        if expires_at <= now || current.pending_email_ciphertext.is_none() {
            if current.revision != expected_revision {
                return Err(DbError::RevisionConflict {
                    expected: Some(expected_revision),
                    actual: Some(current.revision),
                });
            }
            let revision = sqlx::query_scalar::<_, i64>(
                "UPDATE ticket_notification_preferences
                 SET email_code_hash = NULL, email_code_expires_at = NULL,
                     email_code_attempts = 0, revision = revision + 1, updated_at = now()
                 WHERE user_id = $1
                 RETURNING revision",
            )
            .bind(user)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(EmailVerificationOutcome::Expired { revision });
        }
        if current.revision != expected_revision {
            return Err(DbError::RevisionConflict {
                expected: Some(expected_revision),
                actual: Some(current.revision),
            });
        }
        let valid = bool::from(stored_hash.ct_eq(candidate_hash));
        if !valid {
            let attempts = current.email_code_attempts.saturating_add(1);
            let clear = attempts >= 5;
            let revision = sqlx::query_scalar::<_, i64>(
                "UPDATE ticket_notification_preferences
                 SET email_code_hash = CASE WHEN $2 THEN NULL ELSE email_code_hash END,
                     email_code_expires_at = CASE WHEN $2 THEN NULL ELSE email_code_expires_at END,
                     email_code_attempts = CASE WHEN $2 THEN 0 ELSE $3 END,
                     revision = revision + 1, updated_at = now()
                 WHERE user_id = $1
                 RETURNING revision",
            )
            .bind(user)
            .bind(clear)
            .bind(attempts)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(EmailVerificationOutcome::Invalid { revision });
        }
        let revision = sqlx::query_scalar::<_, i64>(
            "UPDATE ticket_notification_preferences
             SET email_ciphertext = pending_email_ciphertext,
                 email_masked = pending_email_masked,
                 pending_email_ciphertext = NULL,
                 pending_email_masked = NULL,
                 email_verified = TRUE,
                 email_enabled = FALSE,
                 email_code_hash = NULL,
                 email_code_expires_at = NULL,
                 email_code_attempts = 0,
                 email_requested_at = NULL,
                 revision = revision + 1,
                 updated_at = now()
             WHERE user_id = $1
             RETURNING revision",
        )
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(EmailVerificationOutcome::Verified { revision })
    }

    pub async fn remove_notification_email(
        &self,
        user: u64,
        expected_revision: i64,
    ) -> Result<i64, DbError> {
        let user = snowflake(user)?;
        let mut tx = self.pool.begin().await?;
        let current = ensure_preferences(&mut tx, user).await?;
        let has_email = current.email_ciphertext.is_some()
            || current.pending_email_ciphertext.is_some()
            || current.email_enabled;
        if !has_email {
            tx.commit().await?;
            return Ok(current.revision);
        }
        if current.revision != expected_revision {
            return Err(DbError::RevisionConflict {
                expected: Some(expected_revision),
                actual: Some(current.revision),
            });
        }
        let revision = sqlx::query_scalar::<_, i64>(
            "UPDATE ticket_notification_preferences
             SET email_enabled = FALSE, email_verified = FALSE,
                 email_ciphertext = NULL, email_masked = NULL,
                 pending_email_ciphertext = NULL, pending_email_masked = NULL,
                 email_code_hash = NULL, email_code_expires_at = NULL,
                 email_code_attempts = 0, email_requested_at = NULL,
                 last_email_request_key = NULL, last_email_request_fingerprint = NULL,
                 revision = revision + 1, updated_at = now()
             WHERE user_id = $1
             RETURNING revision",
        )
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(revision)
    }

    pub async fn insert_gallery_notification(
        &self,
        recipient_id: u64,
        source_guild_id: u64,
        source_event_id: i64,
        event_type: &str,
        submission_title: &str,
        reason: Option<&str>,
        href: &str,
        occurred_at: i64,
    ) -> Result<bool, DbError> {
        if !matches!(event_type, "gallery.approved" | "gallery.rejected")
            || submission_title.trim().is_empty()
            || submission_title.chars().count() > 100
            || reason.is_some_and(|value| value.chars().count() > 500)
            || !href.starts_with("/g/")
            || !href.contains("/tickets/gallery/submissions")
            || href.contains("://")
        {
            return Err(DbError::Invalid("invalid gallery notification".into()));
        }
        let user = snowflake(recipient_id)?;
        let guild = snowflake(source_guild_id)?;
        let mut tx = self.pool.begin().await?;
        let preferences = ensure_preferences(&mut tx, user).await?;
        if !preferences.in_app_enabled {
            tx.commit().await?;
            return Ok(false);
        }
        let inserted = sqlx::query_scalar::<_, i64>(
            "INSERT INTO ticket_notifications
                (user_id, source_event_id, source_guild_id, category, event_type,
                 submission_title, reason, href, created_at)
             VALUES ($1, $2, $3, 'gallery', $4, $5, $6, $7, to_timestamp($8))
             ON CONFLICT (user_id, source_event_id) DO NOTHING
             RETURNING id",
        )
        .bind(user)
        .bind(source_event_id)
        .bind(guild)
        .bind(event_type)
        .bind(submission_title.trim())
        .bind(reason)
        .bind(href)
        .bind(occurred_at)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if inserted {
            bump_revision(&mut tx, user).await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn claim_notification_outbox(
        &self,
        lease_seconds: i64,
    ) -> Result<Option<ClaimedNotificationEvent>, DbError> {
        let row = sqlx::query_as::<_, OutboxRow>(
            "WITH due AS (
                 SELECT event_id
                 FROM ticket_notification_outbox
                 WHERE completed_at IS NULL AND abandoned_at IS NULL
                   AND available_at <= now()
                   AND (locked_until IS NULL OR locked_until <= now())
                 ORDER BY available_at, event_id
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE ticket_notification_outbox AS outbox
             SET attempts = outbox.attempts + 1,
                 locked_until = now() + $1 * interval '1 second'
             FROM due
             JOIN ticket_gallery_events AS event ON event.event_id = due.event_id
             WHERE outbox.event_id = due.event_id
             RETURNING outbox.event_id, outbox.attempts, outbox.in_app_done,
                       outbox.discord_dm_done, outbox.email_done,
                       event.guild_id, event.recipient_id, event.submission_id,
                       event.event_type, event.status, event.reason, event.title,
                       EXTRACT(EPOCH FROM event.occurred_at)::BIGINT AS occurred_at",
        )
        .bind(lease_seconds)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    pub async fn complete_notification_outbox_channel(
        &self,
        event_id: i64,
        attempt: i32,
        channel: NotificationOutboxChannel,
    ) -> Result<bool, DbError> {
        let query = match channel {
            NotificationOutboxChannel::InApp => {
                "UPDATE ticket_notification_outbox SET in_app_done = TRUE
                 WHERE event_id = $1 AND attempts = $2 AND completed_at IS NULL AND abandoned_at IS NULL"
            }
            NotificationOutboxChannel::DiscordDm => {
                "UPDATE ticket_notification_outbox SET discord_dm_done = TRUE
                 WHERE event_id = $1 AND attempts = $2 AND completed_at IS NULL AND abandoned_at IS NULL"
            }
            NotificationOutboxChannel::Email => {
                "UPDATE ticket_notification_outbox SET email_done = TRUE
                 WHERE event_id = $1 AND attempts = $2 AND completed_at IS NULL AND abandoned_at IS NULL"
            }
        };
        let result = sqlx::query(query)
            .bind(event_id)
            .bind(attempt)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn finish_notification_outbox(
        &self,
        event_id: i64,
        attempt: i32,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE ticket_notification_outbox
             SET completed_at = now(), locked_until = NULL, last_failure = NULL
             WHERE event_id = $1 AND attempts = $2
               AND in_app_done AND discord_dm_done AND email_done
               AND completed_at IS NULL AND abandoned_at IS NULL",
        )
        .bind(event_id)
        .bind(attempt)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn retry_notification_outbox(
        &self,
        event_id: i64,
        attempt: i32,
        delay_seconds: i64,
        failure: &'static str,
    ) -> Result<bool, DbError> {
        if !matches!(failure, "transient" | "recipient_unavailable") {
            return Err(DbError::Invalid(
                "invalid notification failure class".into(),
            ));
        }
        let result = sqlx::query(
            "UPDATE ticket_notification_outbox
             SET available_at = now() + $3 * interval '1 second',
                 locked_until = NULL, last_failure = $4
             WHERE event_id = $1 AND attempts = $2
               AND completed_at IS NULL AND abandoned_at IS NULL",
        )
        .bind(event_id)
        .bind(attempt)
        .bind(delay_seconds.max(1))
        .bind(failure)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn abandon_notification_outbox(
        &self,
        event_id: i64,
        attempt: i32,
        failure: &'static str,
    ) -> Result<bool, DbError> {
        if !matches!(
            failure,
            "recipient_unavailable" | "delivery_exhausted" | "permanent"
        ) {
            return Err(DbError::Invalid(
                "invalid notification failure class".into(),
            ));
        }
        let result = sqlx::query(
            "UPDATE ticket_notification_outbox
             SET abandoned_at = now(), locked_until = NULL, last_failure = $3
             WHERE event_id = $1 AND attempts = $2
               AND completed_at IS NULL AND abandoned_at IS NULL",
        )
        .bind(event_id)
        .bind(attempt)
        .bind(failure)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn purge_notification_outbox(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM ticket_notification_outbox
             WHERE completed_at < now() - interval '90 days'
                OR abandoned_at < now() - interval '90 days'",
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
