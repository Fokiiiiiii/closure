use sqlx::{FromRow, Postgres, Transaction};
use subtle::ConstantTimeEq;

use crate::{Db, DbError};

const EXPIRED_VERIFICATION_PURGE_BATCH: i64 = 500;
/// Per recipient address, across all accounts, per window.
pub const EMAIL_QUOTA_PER_RECIPIENT: i64 = 5;
/// Per account, across all of its recipients, per window.
pub const EMAIL_QUOTA_PER_ACCOUNT: i64 = 10;
/// For the whole service, per window.
pub const EMAIL_QUOTA_GLOBAL: i64 = 2000;
pub const EMAIL_QUOTA_WINDOW_SECONDS: i64 = 24 * 60 * 60;
/// Keeps in-app notifications and finished or abandoned outbox rows.
const NOTIFICATION_RETENTION: &str = "365 days";

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
           (email_send_state = 'sent') AS email_code_sent,
           email_send_state,
           email_payload_ciphertext,
           email_code_attempts,
           EXTRACT(EPOCH FROM email_requested_at)::BIGINT AS email_requested_at,
           last_email_request_key,
           last_email_request_fingerprint
    FROM ticket_notification_preferences
    WHERE user_id = $1";

const PREFERENCES_SELECT_FOR_UPDATE: &str = "
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
           (email_send_state = 'sent') AS email_code_sent,
           email_send_state,
           email_payload_ciphertext,
           email_code_attempts,
           EXTRACT(EPOCH FROM email_requested_at)::BIGINT AS email_requested_at,
           last_email_request_key,
           last_email_request_fingerprint
    FROM ticket_notification_preferences
    WHERE user_id = $1
    FOR UPDATE";

#[allow(clippy::struct_excessive_bools)] // Independent stored preference switches.
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
    email_code_sent: bool,
    email_send_state: String,
    email_payload_ciphertext: Option<Vec<u8>>,
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
            email_code_sent: row.email_code_sent,
            email_send_state: row.email_send_state,
            email_payload_ciphertext: row.email_payload_ciphertext,
            email_code_attempts: row.email_code_attempts,
            email_requested_at: row.email_requested_at,
            last_email_request_key: row.last_email_request_key,
            last_email_request_fingerprint: row.last_email_request_fingerprint,
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // Independent notification preference switches.
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
    pub email_code_sent: bool,
    pub email_send_state: String,
    pub email_payload_ciphertext: Option<Vec<u8>>,
    pub email_code_attempts: i16,
    pub email_requested_at: Option<i64>,
    pub last_email_request_key: Option<String>,
    pub last_email_request_fingerprint: Option<Vec<u8>>,
}

impl std::fmt::Debug for StoredNotificationPreferences {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not expose user IDs, email ciphertext, or verification state.
        f.debug_struct("StoredNotificationPreferences")
            .finish_non_exhaustive()
    }
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
            email_code_sent: false,
            email_send_state: "none".to_owned(),
            email_payload_ciphertext: None,
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

impl std::fmt::Debug for NotificationItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Notification text may contain user-provided content.
        f.debug_struct("NotificationItem").finish_non_exhaustive()
    }
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

impl std::fmt::Debug for NotificationPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Nested preferences and notifications are intentionally redacted.
        f.debug_struct("NotificationPage").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub enum EmailRequestOutcome {
    Started {
        revision: i64,
    },
    /// The same request again before its code was delivered: send the stored message, do not issue a new code.
    Resume {
        revision: i64,
        payload: Vec<u8>,
    },
    Replay {
        revision: i64,
    },
    Cooldown {
        retry_after_seconds: u64,
    },
    IdempotencyConflict,
    /// Refused before anything was sent; the cooldown and the history are not changed.
    QuotaExceeded {
        scope: EmailQuotaScope,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailQuotaScope {
    Recipient,
    Account,
    Global,
}

/// Everything one verification request writes, so the call is not a long positional list.
pub struct EmailVerificationRequest {
    pub user: u64,
    pub expected_revision: i64,
    pub email_ciphertext: Vec<u8>,
    pub email_masked: String,
    pub code_hash: Vec<u8>,
    pub code_expires_at: i64,
    pub request_key: String,
    pub request_fingerprint: Vec<u8>,
    /// Sealed `{address, code}`, so a stopped send can be resumed with the same code.
    pub payload_ciphertext: Vec<u8>,
    /// Hash of the normalised address. It is not keyed to one account, so the per-address limit spans accounts.
    pub recipient_hash: Vec<u8>,
    pub now: i64,
}

impl std::fmt::Debug for EmailVerificationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Carries the sealed address, the code hash and the sealed payload.
        f.debug_struct("EmailVerificationRequest")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EmailVerificationOutcome {
    Verified { revision: i64 },
    AlreadyVerified { revision: i64 },
    Invalid { revision: i64 },
    Expired { revision: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationOutboxChannel {
    InApp,
    DiscordDm,
    Email,
}

const ALL_CHANNELS: [NotificationOutboxChannel; 3] = [
    NotificationOutboxChannel::InApp,
    NotificationOutboxChannel::DiscordDm,
    NotificationOutboxChannel::Email,
];

// Column names are fixed per channel, so building SQL from them cannot take user input.
#[derive(Clone, Copy)]
struct ChannelColumns {
    done: &'static str,
    status: &'static str,
    attempts: &'static str,
    next_at: &'static str,
}

impl NotificationOutboxChannel {
    fn columns(self) -> ChannelColumns {
        match self {
            Self::InApp => ChannelColumns {
                done: "in_app_done",
                status: "in_app_status",
                attempts: "in_app_attempts",
                next_at: "in_app_next_at",
            },
            Self::DiscordDm => ChannelColumns {
                done: "discord_dm_done",
                status: "discord_dm_status",
                attempts: "discord_dm_attempts",
                next_at: "discord_dm_next_at",
            },
            Self::Email => ChannelColumns {
                done: "email_done",
                status: "email_status",
                attempts: "email_attempts",
                next_at: "email_next_at",
            },
        }
    }
}

// The event becomes due at the earliest pending channel. `own` replaces that channel's term
// so a write can state its own next time before the row is updated.
fn due_at_sql(own: Option<(NotificationOutboxChannel, &str)>) -> String {
    let terms = ALL_CHANNELS.map(|channel| match own {
        Some((owner, expression)) if owner == channel => expression.to_owned(),
        _ => {
            let columns = channel.columns();
            format!(
                "CASE WHEN {} THEN NULL ELSE {} END",
                columns.done, columns.next_at
            )
        }
    });
    format!("COALESCE(LEAST({}), now())", terms.join(", "))
}

#[allow(clippy::struct_excessive_bools)] // One done flag and one due flag per channel.
#[derive(FromRow)]
struct OutboxRow {
    event_id: i64,
    attempts: i32,
    event_failures: i32,
    in_app_done: bool,
    discord_dm_done: bool,
    email_done: bool,
    in_app_due: bool,
    discord_dm_due: bool,
    email_due: bool,
    in_app_attempts: i32,
    discord_dm_attempts: i32,
    email_attempts: i32,
    guild_id: i64,
    recipient_id: i64,
    submission_id: i64,
    event_type: String,
    status: String,
    reason: Option<String>,
    title: String,
    occurred_at: i64,
}

#[allow(clippy::struct_excessive_bools)] // One done flag and one due flag per channel.
pub struct ClaimedNotificationEvent {
    pub event_id: i64,
    pub attempts: i32,
    pub event_failures: i32,
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
    pub in_app_due: bool,
    pub discord_dm_due: bool,
    pub email_due: bool,
    pub in_app_attempts: i32,
    pub discord_dm_attempts: i32,
    pub email_attempts: i32,
}

impl std::fmt::Debug for ClaimedNotificationEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Delivery records contain recipient IDs and notification content.
        f.debug_struct("ClaimedNotificationEvent")
            .finish_non_exhaustive()
    }
}

impl TryFrom<OutboxRow> for ClaimedNotificationEvent {
    type Error = DbError;

    fn try_from(row: OutboxRow) -> Result<Self, Self::Error> {
        Ok(Self {
            event_id: row.event_id,
            attempts: row.attempts,
            event_failures: row.event_failures,
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
            in_app_due: row.in_app_due,
            discord_dm_due: row.discord_dm_due,
            email_due: row.email_due,
            in_app_attempts: row.in_app_attempts,
            discord_dm_attempts: row.discord_dm_attempts,
            email_attempts: row.email_attempts,
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

    let row = sqlx::query_as::<_, PreferencesRow>(PREFERENCES_SELECT_FOR_UPDATE)
        .bind(user_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(row.into())
}

// Reserves one verification mail under the quota. Returns the exhausted scope, or None once reserved.
async fn reserve_email_send(
    tx: &mut Transaction<'_, Postgres>,
    user: i64,
    recipient_hash: &[u8],
) -> Result<Option<EmailQuotaScope>, DbError> {
    // Serialises reservations so concurrent requests cannot both pass the same count.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('ticket_notification_email_quota'))")
        .execute(&mut **tx)
        .await?;
    let (recipient_sends, account_sends, global_sends): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE recipient_hash = $1),
                count(*) FILTER (WHERE account_id = $2),
                count(*)
         FROM ticket_notification_email_sends
         WHERE reserved_at > now() - $3 * interval '1 second'",
    )
    .bind(recipient_hash)
    .bind(user)
    .bind(EMAIL_QUOTA_WINDOW_SECONDS)
    .fetch_one(&mut **tx)
    .await?;
    let exceeded = if recipient_sends >= EMAIL_QUOTA_PER_RECIPIENT {
        Some(EmailQuotaScope::Recipient)
    } else if account_sends >= EMAIL_QUOTA_PER_ACCOUNT {
        Some(EmailQuotaScope::Account)
    } else if global_sends >= EMAIL_QUOTA_GLOBAL {
        Some(EmailQuotaScope::Global)
    } else {
        None
    };
    if exceeded.is_none() {
        sqlx::query(
            "INSERT INTO ticket_notification_email_sends (account_id, recipient_hash)
             VALUES ($1, $2)",
        )
        .bind(user)
        .bind(recipient_hash)
        .execute(&mut **tx)
        .await?;
    }
    Ok(exceeded)
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
        request: EmailVerificationRequest,
    ) -> Result<EmailRequestOutcome, DbError> {
        let user = snowflake(request.user)?;
        if request.code_hash.len() != 32 || request.request_fingerprint.len() != 32 {
            return Err(DbError::Invalid("invalid email verification state".into()));
        }
        let mut tx = self.pool.begin().await?;
        let current = ensure_preferences(&mut tx, user).await?;

        if current.last_email_request_key.as_deref() == Some(request.request_key.as_str()) {
            let matches = current
                .last_email_request_fingerprint
                .as_deref()
                .is_some_and(|stored| bool::from(stored.ct_eq(&request.request_fingerprint)));
            if !matches {
                return Ok(EmailRequestOutcome::IdempotencyConflict);
            }
            let live = current
                .email_code_expires_at
                .is_some_and(|expires| expires > request.now);
            if current.email_verified || (live && current.email_send_state == "sent") {
                tx.commit().await?;
                return Ok(EmailRequestOutcome::Replay {
                    revision: current.revision,
                });
            }
            // A send that did not complete resumes with the code it already issued: a new code
            // would make a mail that may be in flight unusable.
            let resumable =
                live && matches!(current.email_send_state.as_str(), "queued" | "failed");
            if let (true, Some(payload)) = (resumable, current.email_payload_ciphertext.clone()) {
                tx.commit().await?;
                return Ok(EmailRequestOutcome::Resume {
                    revision: current.revision,
                    payload,
                });
            }
        }

        if let Some(requested_at) = current.email_requested_at {
            let elapsed = request.now.saturating_sub(requested_at).clamp(0, 60);
            if elapsed < 60 {
                tx.commit().await?;
                return Ok(EmailRequestOutcome::Cooldown {
                    retry_after_seconds: u64::try_from(60 - elapsed).unwrap_or(60),
                });
            }
        }
        if current.revision != request.expected_revision {
            return Err(DbError::RevisionConflict {
                expected: Some(request.expected_revision),
                actual: Some(current.revision),
            });
        }

        if let Some(scope) = reserve_email_send(&mut tx, user, &request.recipient_hash).await? {
            tx.commit().await?;
            return Ok(EmailRequestOutcome::QuotaExceeded { scope });
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
                 email_code_sent_at = NULL,
                 email_send_state = 'queued',
                 email_payload_ciphertext = $9,
                 email_requested_at = to_timestamp($6),
                 last_email_request_key = $7,
                 last_email_request_fingerprint = $8,
                 revision = revision + 1,
                 updated_at = now()
             WHERE user_id = $1
             RETURNING revision",
        )
        .bind(user)
        .bind(request.email_ciphertext)
        .bind(request.email_masked)
        .bind(request.code_hash)
        .bind(request.code_expires_at)
        .bind(request.now)
        .bind(request.request_key)
        .bind(request.request_fingerprint)
        .bind(request.payload_ciphertext)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(EmailRequestOutcome::Started { revision })
    }

    // Keeps the key, the cooldown and the payload, so the same request can resume the send.
    pub async fn mark_email_verification_failed(
        &self,
        user: u64,
        request_key: &str,
    ) -> Result<(), DbError> {
        let user = snowflake(user)?;
        sqlx::query(
            "UPDATE ticket_notification_preferences
             SET email_send_state = 'failed'
             WHERE user_id = $1 AND last_email_request_key = $2 AND email_send_state = 'queued'",
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
                 SET email_send_state = 'none', email_payload_ciphertext = NULL,
                      email_code_hash = NULL, email_code_expires_at = NULL,
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
             SET email_send_state = 'none', email_payload_ciphertext = NULL,
                  email_ciphertext = pending_email_ciphertext,
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
                  email_send_state = 'none', email_payload_ciphertext = NULL,
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

    // These payload fields map independently to the notification row.
    #[allow(clippy::too_many_arguments)]
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
             RETURNING outbox.event_id, outbox.attempts, outbox.event_failures,
                       outbox.in_app_done, outbox.discord_dm_done, outbox.email_done,
                       outbox.in_app_next_at <= now() AS in_app_due,
                       outbox.discord_dm_next_at <= now() AS discord_dm_due,
                       outbox.email_next_at <= now() AS email_due,
                       outbox.in_app_attempts, outbox.discord_dm_attempts, outbox.email_attempts,
                       event.guild_id, event.recipient_id, event.submission_id,
                       event.event_type, event.status, event.reason, event.title,
                       EXTRACT(EPOCH FROM event.occurred_at)::BIGINT AS occurred_at",
        )
        .bind(lease_seconds)
        .fetch_optional(&self.pool)
        .await?;
        row.map(TryInto::try_into).transpose()
    }

    // The channel outcome is stored with the same lease check as every other write, so a
    // stale worker cannot record a result for a lease it no longer holds.
    pub async fn complete_notification_outbox_channel(
        &self,
        event_id: i64,
        attempt: i32,
        channel: NotificationOutboxChannel,
        status: &'static str,
    ) -> Result<bool, DbError> {
        if !matches!(status, "sent" | "skipped" | "failed") {
            return Err(DbError::Invalid("invalid channel outcome".into()));
        }
        let columns = channel.columns();
        let query = format!(
            "UPDATE ticket_notification_outbox
             SET {done} = TRUE, {status_column} = $3, available_at = {due}
             WHERE event_id = $1 AND attempts = $2 AND NOT {done}
               AND completed_at IS NULL AND abandoned_at IS NULL",
            done = columns.done,
            status_column = columns.status,
            due = due_at_sql(Some((channel, "NULL"))),
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(event_id)
            .bind(attempt)
            .bind(status)
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

    // Counts one failed attempt against this channel only. Once the channel has used its
    // attempts it is given up as failed, and the other channels are not held back.
    pub async fn retry_notification_channel(
        &self,
        event_id: i64,
        attempt: i32,
        channel: NotificationOutboxChannel,
        status: &'static str,
        delay_seconds: i64,
        max_attempts: i32,
    ) -> Result<bool, DbError> {
        if !matches!(status, "pending" | "unknown") {
            return Err(DbError::Invalid("invalid channel retry state".into()));
        }
        let columns = channel.columns();
        let exhausted = format!("{} + 1 >= $4", columns.attempts);
        let query = format!(
            "UPDATE ticket_notification_outbox
             SET {attempts} = {attempts} + 1,
                 {next_at} = now() + $3 * interval '1 second',
                 {done} = ({exhausted}),
                 {status_column} = CASE WHEN {exhausted} THEN 'failed' ELSE $5 END,
                 available_at = {due}
             WHERE event_id = $1 AND attempts = $2 AND NOT {done}
               AND completed_at IS NULL AND abandoned_at IS NULL",
            attempts = columns.attempts,
            next_at = columns.next_at,
            done = columns.done,
            status_column = columns.status,
            exhausted = exhausted,
            due = due_at_sql(Some((
                channel,
                &format!(
                    "CASE WHEN {exhausted} THEN NULL ELSE now() + $3 * interval '1 second' END"
                ),
            ))),
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(event_id)
            .bind(attempt)
            .bind(delay_seconds.max(1))
            .bind(max_attempts)
            .bind(status)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    // Waiting for configuration is not a failed delivery, so no attempt is counted.
    pub async fn defer_notification_channel(
        &self,
        event_id: i64,
        attempt: i32,
        channel: NotificationOutboxChannel,
        delay_seconds: i64,
    ) -> Result<bool, DbError> {
        let columns = channel.columns();
        let query = format!(
            "UPDATE ticket_notification_outbox
             SET {next_at} = now() + $3 * interval '1 second',
                 available_at = {due}
             WHERE event_id = $1 AND attempts = $2 AND NOT {done}
               AND completed_at IS NULL AND abandoned_at IS NULL",
            next_at = columns.next_at,
            done = columns.done,
            due = due_at_sql(Some((channel, "now() + $3 * interval '1 second'",))),
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(event_id)
            .bind(attempt)
            .bind(delay_seconds.max(1))
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    // Returns the lease after a pass that did not finish the event.
    pub async fn release_notification_outbox(
        &self,
        event_id: i64,
        attempt: i32,
    ) -> Result<bool, DbError> {
        let query = format!(
            "UPDATE ticket_notification_outbox
             SET locked_until = NULL, available_at = {due}
             WHERE event_id = $1 AND attempts = $2
               AND completed_at IS NULL AND abandoned_at IS NULL",
            due = due_at_sql(None),
        );
        let result = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(event_id)
            .bind(attempt)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    // Event-level failures (member lookup, storage) are counted apart from channel attempts.
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
                 locked_until = NULL, last_failure = $4,
                 event_failures = event_failures + 1
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

    pub async fn mark_email_verification_sent(
        &self,
        user: u64,
        request_key: &str,
    ) -> Result<(), DbError> {
        let user = snowflake(user)?;
        sqlx::query(
            "UPDATE ticket_notification_preferences
             SET email_code_sent_at = now(), email_send_state = 'sent'
             WHERE user_id = $1 AND last_email_request_key = $2 AND email_code_sent_at IS NULL",
        )
        .bind(user)
        .bind(request_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn purge_expired_email_verifications(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE ticket_notification_preferences
             SET pending_email_ciphertext = NULL,
                 pending_email_masked = NULL,
                 email_code_hash = NULL,
                 email_code_expires_at = NULL,
                 email_code_attempts = 0,
                 email_code_sent_at = NULL,
                 email_send_state = 'none', email_payload_ciphertext = NULL,
                 email_requested_at = NULL,
                 last_email_request_key = NULL,
                 last_email_request_fingerprint = NULL,
                 revision = revision + 1,
                 updated_at = now()
             WHERE user_id IN (
                 SELECT user_id
                 FROM ticket_notification_preferences
                 WHERE pending_email_ciphertext IS NOT NULL
                   AND (email_code_expires_at IS NULL OR email_code_expires_at <= now())
                 LIMIT $1
             )",
        )
        .bind(EXPIRED_VERIFICATION_PURGE_BATCH)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    // Only the last window is needed for the quota; older reservations are removed in bounded batches.
    pub async fn purge_email_send_history(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM ticket_notification_email_sends
             WHERE id IN (
                 SELECT id FROM ticket_notification_email_sends
                 WHERE reserved_at < now() - $1 * interval '1 second'
                 ORDER BY id
                 LIMIT $2
             )",
        )
        .bind(EMAIL_QUOTA_WINDOW_SECONDS)
        .bind(EXPIRED_VERIFICATION_PURGE_BATCH)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn purge_notification_outbox(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM ticket_notification_outbox
             WHERE event_id IN (
                 SELECT event_id FROM ticket_notification_outbox
                 WHERE completed_at < now() - $1::interval
                    OR abandoned_at < now() - $1::interval
                 ORDER BY event_id
                 LIMIT $2
             )",
        )
        .bind(NOTIFICATION_RETENTION)
        .bind(EXPIRED_VERIFICATION_PURGE_BATCH)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn purge_expired_notifications(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM ticket_notifications
             WHERE id IN (
                 SELECT id FROM ticket_notifications
                 WHERE created_at < now() - $1::interval
                 ORDER BY id
                 LIMIT $2
             )",
        )
        .bind(NOTIFICATION_RETENTION)
        .bind(EXPIRED_VERIFICATION_PURGE_BATCH)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    async fn database(pool: PgPool) -> Db {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        db
    }

    async fn start(
        db: &Db,
        user: u64,
        expected_revision: i64,
        key: &str,
        seed: u8,
        now: i64,
    ) -> EmailRequestOutcome {
        db.begin_email_verification(EmailVerificationRequest {
            user,
            expected_revision,
            email_ciphertext: vec![1],
            email_masked: "a***@example.com".to_owned(),
            code_hash: vec![seed; 32],
            code_expires_at: now + 900,
            request_key: key.to_owned(),
            request_fingerprint: vec![seed; 32],
            payload_ciphertext: vec![seed, 9],
            recipient_hash: vec![seed; 32],
            now,
        })
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = false)]
    async fn an_unsent_request_resumes_with_the_stored_payload_when_replayed(pool: PgPool) {
        let db = database(pool).await;
        let EmailRequestOutcome::Started { revision } = start(&db, 7, 0, "req-1", 3, 1_000).await
        else {
            panic!("the first request starts");
        };
        // The process stopped before the code was delivered; the same request arrives again.
        let replay = start(&db, 7, revision, "req-1", 3, 1_005).await;
        let EmailRequestOutcome::Resume {
            revision: resumed_revision,
            payload,
        } = replay
        else {
            panic!("an unsent request resumes, got {replay:?}");
        };
        assert_eq!((resumed_revision, payload), (revision, vec![3, 9]));
    }

    #[sqlx::test(migrations = false)]
    async fn a_failed_send_resumes_under_its_key_and_keeps_the_cooldown(pool: PgPool) {
        let db = database(pool).await;
        let EmailRequestOutcome::Started { revision } = start(&db, 7, 0, "req-1", 3, 1_000).await
        else {
            panic!("the first request starts");
        };
        db.mark_email_verification_failed(7, "req-1").await.unwrap();
        let resumed = start(&db, 7, revision, "req-1", 3, 1_010).await;
        assert!(
            matches!(resumed, EmailRequestOutcome::Resume { .. }),
            "a failed send resumes under the same key, got {resumed:?}"
        );
        let current = db.notification_preferences(7).await.unwrap().revision;
        let other_key = start(&db, 7, current, "req-2", 4, 1_010).await;
        assert!(
            matches!(
                other_key,
                EmailRequestOutcome::Cooldown {
                    retry_after_seconds: 50
                }
            ),
            "a failed send must not reopen the 60 second cooldown, got {other_key:?}"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn a_replay_of_a_delivered_code_is_not_sent_again(pool: PgPool) {
        let db = database(pool).await;
        let EmailRequestOutcome::Started { revision } = start(&db, 7, 0, "req-1", 3, 1_000).await
        else {
            panic!("the first request starts");
        };
        db.mark_email_verification_sent(7, "req-1").await.unwrap();
        let replay = start(&db, 7, revision, "req-1", 3, 1_005).await;
        assert!(
            matches!(replay, EmailRequestOutcome::Replay { .. }),
            "a delivered code is answered as a replay, got {replay:?}"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn an_expired_unsent_request_gets_a_fresh_code(pool: PgPool) {
        let db = database(pool).await;
        let EmailRequestOutcome::Started { revision } = start(&db, 7, 0, "req-1", 3, 1_000).await
        else {
            panic!("the first request starts");
        };
        // 1_000 + 900 has passed, so the stored code can no longer be used.
        let replay = start(&db, 7, revision, "req-1", 3, 2_000).await;
        assert!(
            matches!(replay, EmailRequestOutcome::Started { .. }),
            "an expired unsent request gets a fresh code, got {replay:?}"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn only_expired_pending_addresses_are_purged(pool: PgPool) {
        let db = database(pool).await;
        start(&db, 7, 0, "req-7", 3, 1_000).await;
        db.begin_email_verification(EmailVerificationRequest {
            user: 8,
            expected_revision: 0,
            email_ciphertext: vec![1],
            email_masked: "a***@example.com".to_owned(),
            code_hash: vec![4; 32],
            code_expires_at: 4_000_000_000,
            request_key: "req-8".to_owned(),
            request_fingerprint: vec![4; 32],
            payload_ciphertext: vec![4, 9],
            recipient_hash: vec![4; 32],
            now: 1_000,
        })
        .await
        .unwrap();

        assert_eq!(db.purge_expired_email_verifications().await.unwrap(), 1);

        let expired = db.notification_preferences(7).await.unwrap();
        assert!(expired.pending_email_ciphertext.is_none());
        assert!(expired.email_code_hash.is_none());
        assert!(expired.email_payload_ciphertext.is_none());
        let active = db.notification_preferences(8).await.unwrap();
        assert!(active.pending_email_ciphertext.is_some());
        assert!(active.email_code_hash.is_some());
    }

    async fn seed_event(pool: &PgPool, recipient: i64) -> i64 {
        let submission: i64 = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_templates
                (kind, title, description, category, payload, author_id, source_guild_id, author_name, status)
             VALUES ('panel', 'Title', 'Description', 'support', '{}'::jsonb, 1, 10, 'author', 'approved')
             RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let event: i64 = sqlx::query_scalar(
            "INSERT INTO ticket_gallery_events
                (submission_id, guild_id, revision, recipient_id, reviewer_id, event_type, status, title)
             VALUES ($1, 10, 1, $2, 2, 'gallery.approved', 'approved', 'Title')
             RETURNING event_id",
        )
        .bind(submission)
        .bind(recipient)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO ticket_notification_outbox (event_id) VALUES ($1)")
            .bind(event)
            .execute(pool)
            .await
            .unwrap();
        event
    }

    async fn claim(db: &Db) -> ClaimedNotificationEvent {
        db.claim_notification_outbox(60).await.unwrap().unwrap()
    }

    #[sqlx::test(migrations = false)]
    async fn a_failed_channel_backs_off_without_moving_the_others(pool: PgPool) {
        let db = database(pool.clone()).await;
        let event_id = seed_event(&pool, 42).await;
        let event = claim(&db).await;
        assert!(event.discord_dm_due && event.in_app_due && event.email_due);

        assert!(
            db.retry_notification_channel(
                event_id,
                event.attempts,
                NotificationOutboxChannel::DiscordDm,
                "pending",
                300,
                10,
            )
            .await
            .unwrap()
        );

        let (dm_attempts, dm_status, dm_later, in_app_status, in_app_attempts, due_now): (
            i32,
            String,
            bool,
            String,
            i32,
            bool,
        ) = sqlx::query_as(
            "SELECT discord_dm_attempts, discord_dm_status,
                    discord_dm_next_at > now() + interval '200 seconds',
                    in_app_status, in_app_attempts, available_at <= now()
             FROM ticket_notification_outbox WHERE event_id = $1",
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (dm_attempts, dm_status.as_str(), dm_later),
            (1, "pending", true)
        );
        assert_eq!((in_app_status.as_str(), in_app_attempts), ("pending", 0));
        assert!(due_now, "the other pending channels must stay due");
    }

    #[sqlx::test(migrations = false)]
    async fn a_channel_that_uses_its_attempts_fails_and_the_event_still_completes(pool: PgPool) {
        let db = database(pool.clone()).await;
        let event_id = seed_event(&pool, 42).await;
        for _ in 0..3 {
            sqlx::query(
                "UPDATE ticket_notification_outbox
                 SET available_at = now(), locked_until = NULL, discord_dm_next_at = now()",
            )
            .execute(&pool)
            .await
            .unwrap();
            let event = claim(&db).await;
            assert!(
                db.retry_notification_channel(
                    event_id,
                    event.attempts,
                    NotificationOutboxChannel::DiscordDm,
                    "pending",
                    1,
                    3,
                )
                .await
                .unwrap()
            );
            assert!(
                db.release_notification_outbox(event_id, event.attempts)
                    .await
                    .unwrap()
            );
        }
        let (dm_status, dm_done, abandoned): (String, bool, bool) = sqlx::query_as(
            "SELECT discord_dm_status, discord_dm_done, abandoned_at IS NOT NULL
             FROM ticket_notification_outbox WHERE event_id = $1",
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (dm_status.as_str(), dm_done, abandoned),
            ("failed", true, false)
        );

        sqlx::query(
            "UPDATE ticket_notification_outbox SET available_at = now(), locked_until = NULL",
        )
        .execute(&pool)
        .await
        .unwrap();
        let event = claim(&db).await;
        assert!(
            db.complete_notification_outbox_channel(
                event_id,
                event.attempts,
                NotificationOutboxChannel::InApp,
                "sent"
            )
            .await
            .unwrap()
        );
        assert!(
            db.complete_notification_outbox_channel(
                event_id,
                event.attempts,
                NotificationOutboxChannel::Email,
                "skipped"
            )
            .await
            .unwrap()
        );
        assert!(
            db.finish_notification_outbox(event_id, event.attempts)
                .await
                .unwrap()
        );
    }

    #[sqlx::test(migrations = false)]
    async fn an_email_send_with_unknown_outcome_is_kept_as_unknown(pool: PgPool) {
        let db = database(pool.clone()).await;
        let event_id = seed_event(&pool, 42).await;
        let event = claim(&db).await;
        assert!(
            db.retry_notification_channel(
                event_id,
                event.attempts,
                NotificationOutboxChannel::Email,
                "unknown",
                5,
                10,
            )
            .await
            .unwrap()
        );
        let (status, done): (String, bool) = sqlx::query_as(
            "SELECT email_status, email_done FROM ticket_notification_outbox WHERE event_id = $1",
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((status.as_str(), done), ("unknown", false));
    }

    #[sqlx::test(migrations = false)]
    async fn a_channel_outcome_outside_the_known_states_is_rejected(pool: PgPool) {
        let db = database(pool.clone()).await;
        let event_id = seed_event(&pool, 42).await;
        let event = claim(&db).await;
        assert!(
            db.complete_notification_outbox_channel(
                event_id,
                event.attempts,
                NotificationOutboxChannel::InApp,
                "delivered"
            )
            .await
            .is_err()
        );
    }

    async fn request_to(
        db: &Db,
        user: u64,
        key: &str,
        recipient: u8,
        now: i64,
    ) -> EmailRequestOutcome {
        let expected_revision = db.notification_preferences(user).await.unwrap().revision;
        db.begin_email_verification(EmailVerificationRequest {
            user,
            expected_revision,
            email_ciphertext: vec![1],
            email_masked: "a***@example.com".to_owned(),
            code_hash: vec![recipient; 32],
            code_expires_at: now + 900,
            request_key: key.to_owned(),
            request_fingerprint: vec![recipient; 32],
            payload_ciphertext: vec![recipient, 9],
            recipient_hash: vec![recipient; 32],
            now,
        })
        .await
        .unwrap()
    }

    async fn history_rows(pool: &PgPool, recipient: u8) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM ticket_notification_email_sends WHERE recipient_hash = $1",
        )
        .bind(vec![recipient; 32])
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = false)]
    async fn one_address_is_limited_across_accounts(pool: PgPool) {
        let db = database(pool.clone()).await;
        for index in 0..EMAIL_QUOTA_PER_RECIPIENT {
            let user = if index % 2 == 0 { 7 } else { 8 };
            let outcome =
                request_to(&db, user, &format!("req-{index}"), 3, 1_000 + index * 61).await;
            assert!(
                matches!(outcome, EmailRequestOutcome::Started { .. }),
                "{outcome:?}"
            );
        }
        let refused = request_to(&db, 9, "req-over", 3, 1_000 + 5 * 61).await;
        assert!(
            matches!(
                refused,
                EmailRequestOutcome::QuotaExceeded {
                    scope: EmailQuotaScope::Recipient
                }
            ),
            "the sixth mail to one address is refused, got {refused:?}"
        );
        assert_eq!(history_rows(&pool, 3).await, EMAIL_QUOTA_PER_RECIPIENT);
    }

    #[sqlx::test(migrations = false)]
    async fn one_account_is_limited_across_recipients(pool: PgPool) {
        let db = database(pool.clone()).await;
        for index in 0..EMAIL_QUOTA_PER_ACCOUNT {
            let outcome = request_to(
                &db,
                7,
                &format!("req-{index}"),
                10 + u8::try_from(index).unwrap(),
                1_000 + index * 61,
            )
            .await;
            assert!(
                matches!(outcome, EmailRequestOutcome::Started { .. }),
                "{outcome:?}"
            );
        }
        let refused = request_to(&db, 7, "req-over", 99, 1_000 + 10 * 61).await;
        assert!(
            matches!(
                refused,
                EmailRequestOutcome::QuotaExceeded {
                    scope: EmailQuotaScope::Account
                }
            ),
            "the eleventh mail from one account is refused, got {refused:?}"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn the_whole_service_has_a_window_limit(pool: PgPool) {
        let db = database(pool.clone()).await;
        sqlx::query(
            "INSERT INTO ticket_notification_email_sends (account_id, recipient_hash)
             SELECT 100 + g, sha256(convert_to('batch-' || g::text, 'UTF8'))
             FROM generate_series(1, $1) AS g",
        )
        .bind(EMAIL_QUOTA_GLOBAL)
        .execute(&pool)
        .await
        .unwrap();

        let refused = request_to(&db, 7, "req-1", 42, 1_000).await;
        assert!(
            matches!(
                refused,
                EmailRequestOutcome::QuotaExceeded {
                    scope: EmailQuotaScope::Global
                }
            ),
            "the service-wide limit applies, got {refused:?}"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn a_refused_request_leaves_no_history_and_a_failed_send_still_counts(pool: PgPool) {
        let db = database(pool.clone()).await;
        for index in 0..EMAIL_QUOTA_PER_RECIPIENT {
            request_to(&db, 7, &format!("req-{index}"), 5, 1_000 + index * 61).await;
        }
        let refused = request_to(&db, 7, "req-over", 5, 1_000 + 5 * 61).await;
        assert!(matches!(refused, EmailRequestOutcome::QuotaExceeded { .. }));
        assert_eq!(history_rows(&pool, 5).await, EMAIL_QUOTA_PER_RECIPIENT);

        // A send that failed after the reservation still uses its quota.
        db.mark_email_verification_failed(7, "req-0").await.unwrap();
        assert_eq!(history_rows(&pool, 5).await, EMAIL_QUOTA_PER_RECIPIENT);
    }

    #[sqlx::test(migrations = false)]
    async fn history_older_than_the_window_is_purged_in_bounded_batches(pool: PgPool) {
        let db = database(pool.clone()).await;
        sqlx::query(
            "INSERT INTO ticket_notification_email_sends (account_id, recipient_hash, reserved_at)
             SELECT 1, sha256(convert_to('old-' || g::text, 'UTF8')),
                    now() - interval '2 days'
             FROM generate_series(1, 600) AS g",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ticket_notification_email_sends (account_id, recipient_hash)
             VALUES (1, sha256('recent'::bytea))",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(db.purge_email_send_history().await.unwrap(), 500);
        let remaining: i64 =
            sqlx::query_scalar("SELECT count(*) FROM ticket_notification_email_sends")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(remaining, 101);
    }

    #[sqlx::test(migrations = false)]
    async fn a_finished_outbox_row_is_kept_for_a_year(pool: PgPool) {
        let db = database(pool.clone()).await;
        let event_id = seed_event(&pool, 42).await;
        sqlx::query(
            "UPDATE ticket_notification_outbox
             SET completed_at = now() - interval '100 days'
             WHERE event_id = $1",
        )
        .bind(event_id)
        .execute(&pool)
        .await
        .unwrap();

        db.purge_notification_outbox().await.unwrap();

        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM ticket_notification_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            remaining, 1,
            "a row finished 100 days ago is inside the 365 day retention"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn in_app_notifications_are_purged_after_a_year_in_bounded_batches(pool: PgPool) {
        let db = database(pool.clone()).await;
        sqlx::query(
            "INSERT INTO ticket_notifications
                (user_id, source_event_id, source_guild_id, category, event_type,
                 submission_title, href, created_at)
             SELECT 42, g, 10, 'gallery', 'gallery.approved', 'Title',
                    '/g/10/tickets/gallery/submissions', now() - interval '400 days'
             FROM generate_series(1, 600) AS g",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ticket_notifications
                (user_id, source_event_id, source_guild_id, category, event_type,
                 submission_title, href, created_at)
             VALUES (42, 1000, 10, 'gallery', 'gallery.approved', 'Title',
                     '/g/10/tickets/gallery/submissions', now() - interval '364 days')",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(db.purge_expired_notifications().await.unwrap(), 500);
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM ticket_notifications")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            remaining, 101,
            "only the rows older than 365 days are removed"
        );
    }
}
