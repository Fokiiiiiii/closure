//! PostgreSQL persistence. The database is the source of truth for guild configuration. Every write is
//! one transaction that also appends history and notifies other processes, configuration edits use
//! optimistic concurrency (a stale editor gets a conflict instead of silently overwriting), and queries
//! are parameterised and exercised against a real database in the tests.

mod audit;
mod cases;
mod config;
mod error;
mod guilds;
mod history;
mod integration_secrets;
pub mod notifications;

#[cfg(test)]
#[path = "history_file_tests.rs"]
mod history_file_tests;
mod ids;
mod requests;
pub use requests::{Claim, REQUESTS_KEPT};
mod scheduler;
pub use scheduler::{ScheduledPost, Withdrawn};
pub use ticket_gallery::{
    GalleryCard, GalleryError, GalleryFilter, GalleryPage, GallerySubmission, GalleryTemplate,
    NewGalleryTemplate,
};
mod stats;
mod storage;
mod ticket_gallery;

use std::str::FromStr;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub use audit::{AuditEvent, AuditMetadata, AuditRecord};
pub use config::{CONFIG_CHANNEL, ConfigEvent, ConfigHistoryEntry, ConfigListener, StoredConfig};
pub use error::DbError;

#[derive(Debug, Clone)]
pub struct DbOptions {
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub statement_timeout: Duration,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            max_connections: 10,
            acquire_timeout: Duration::from_secs(5),
            statement_timeout: Duration::from_secs(10),
        }
    }
}

/// Handle to the database; cheap to clone.
#[derive(Debug, Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connects to `url`. The URL carries credentials and is never logged.
    pub async fn connect(url: &str, options: &DbOptions) -> Result<Self, DbError> {
        let connect_options = PgConnectOptions::from_str(url)?
            .application_name("closure")
            .options([(
                "statement_timeout",
                options.statement_timeout.as_millis().to_string(),
            )]);
        let pool = PgPoolOptions::new()
            .max_connections(options.max_connections)
            .acquire_timeout(options.acquire_timeout)
            .connect_with(connect_options)
            .await?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool (used by tests).
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Applies pending migrations. Safe to call on every start.
    pub async fn migrate(&self) -> Result<(), DbError> {
        sqlx::migrate!().run(&self.pool).await?;
        Ok(())
    }

    /// Verifies the database answers; used for readiness checks.
    pub async fn ping(&self) -> Result<(), DbError> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn malformed_urls_are_rejected_without_connecting() {
        assert!(
            Db::connect("not a url", &DbOptions::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn connect_applies_the_statement_timeout_and_answers_pings() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a test server");
        let options = DbOptions {
            max_connections: 1,
            statement_timeout: Duration::from_secs(7),
            ..DbOptions::default()
        };
        let db = Db::connect(&url, &options).await.unwrap();
        db.ping().await.unwrap();

        let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(timeout, "7s");
        db.close().await;
    }

    #[sqlx::test(migrations = false)]
    async fn migrating_is_idempotent_and_creates_the_schema(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        db.migrate().await.unwrap();

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text FROM information_schema.tables
             WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations'
             ORDER BY table_name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            tables,
            [
                "api_requests",
                "audit_events",
                "case_edits",
                "cases",
                "dm_component_routes",
                "guild_case_numbers",
                "guild_config",
                "guild_config_history",
                "guild_member_activity",
                "guilds",
                "integration_secret_generations",
                "integration_secret_receipts",
                "integration_secret_tombstones",
                "integration_secrets",
                "message_file_usage",
                "message_files",
                "module_counters",
                "module_kv",
                "module_kv_usage",
                "module_scores",
                "module_sets",
                "saved_messages",
                "scheduled_jobs",
                "ticket_gallery_events",
                "ticket_gallery_installations",
                "ticket_gallery_requests",
                "ticket_gallery_templates",
                "ticket_notification_email_sends",
                "ticket_notification_outbox",
                "ticket_notification_preferences",
                "ticket_notifications"
            ]
        );
    }

    #[sqlx::test(migrations = false)]
    async fn the_entries_of_the_plugins_move_to_scores_and_sets(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        sqlx::query("INSERT INTO module_scores VALUES ('leveling', 1, 'xp', 7, 5)")
            .execute(&pool)
            .await
            .unwrap();
        for (module, key, value) in [
            ("leveling", "xp:7", r#"{"xp": 100, "last": 1}"#),
            ("leveling", "xp:8", r#"{"xp": 40, "last": 1}"#),
            ("leveling", "xp:9", r#"{"xp": 0, "last": 1}"#),
            ("giveaway", "e:55:7", "1"),
            ("giveaway", "e:55:8", "1"),
            ("starboard", "post:300", r#"{"board": 20, "post": 301}"#),
            ("leveling", "other", "1"),
        ] {
            sqlx::query(
                "INSERT INTO module_kv (module, guild_id, key, value, revision)
                 VALUES ($1, 1, $2, $3::jsonb, nextval('module_kv_revisions'))",
            )
            .bind(module)
            .bind(key)
            .bind(value)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::raw_sql(include_str!(
            "../migrations/0015_backfill_scores_and_sets.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let xp: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT subject, value FROM module_scores WHERE name = 'xp' ORDER BY subject",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            xp,
            [(7, 105), (8, 40)],
            "added to what was there, nothing for no experience"
        );
        let entered: Vec<i64> = sqlx::query_scalar(
            "SELECT member FROM module_sets WHERE name = 'e-55' ORDER BY member",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(entered, [7, 8]);
        let post: i64 = sqlx::query_scalar(
            "SELECT value FROM module_scores WHERE module = 'starboard' AND subject = 300",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(post, 301);
        let left: Vec<String> = sqlx::query_scalar("SELECT key FROM module_kv ORDER BY key")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            left,
            ["other"],
            "the entries of the three plugins are gone, the rest stays"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn the_entries_with_19_digit_ids_that_0015_left_are_moved_by_0019(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        // A 19-digit id, the largest id a BIGINT holds, one above it, and an entry that cannot
        // be copied (it names no experience).
        for (module, key, value) in [
            ("leveling", "xp:1300000000000000000", r#"{"xp": 50}"#),
            ("leveling", "xp:9223372036854775807", r#"{"xp": 7}"#),
            ("leveling", "xp:9223372036854775808", r#"{"xp": 9}"#),
            ("leveling", "xp:1300000000000000001", r#"{"last": 1}"#),
            ("giveaway", "e:1400000000000000000:1300000000000000000", "1"),
            (
                "starboard",
                "post:300",
                r#"{"board": 20, "post": 1500000000000000000}"#,
            ),
        ] {
            sqlx::query(
                "INSERT INTO module_kv (module, guild_id, key, value, revision)
                 VALUES ($1, 1, $2, $3::jsonb, nextval('module_kv_revisions'))",
            )
            .bind(module)
            .bind(key)
            .bind(value)
            .execute(&pool)
            .await
            .unwrap();
        }
        // The migration's helper lives in the temporary schema of the connection that ran it, and
        // the pool may hand that very connection out again: clear it on the one used here.
        let mut connection = pool.acquire().await.unwrap();
        sqlx::raw_sql("DROP FUNCTION IF EXISTS pg_temp.fitting_id(text)")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0019_backfill_long_ids.sql"))
            .execute(&mut *connection)
            .await
            .unwrap();
        drop(connection);

        let xp: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT subject, value FROM module_scores WHERE name = 'xp' ORDER BY subject",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(xp, [(1_300_000_000_000_000_000, 50), (i64::MAX, 7)]);
        let entered: Vec<(String, i64)> =
            sqlx::query_as("SELECT name, member FROM module_sets ORDER BY member")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            entered,
            [(
                "e-1400000000000000000".to_owned(),
                1_300_000_000_000_000_000
            )]
        );
        let post: i64 = sqlx::query_scalar(
            "SELECT value FROM module_scores WHERE module = 'starboard' AND subject = 300",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(post, 1_500_000_000_000_000_000);
        let left: Vec<String> = sqlx::query_scalar("SELECT key FROM module_kv ORDER BY key")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(
            left,
            ["xp:1300000000000000001", "xp:9223372036854775808"],
            "what cannot be copied is kept, not lost"
        );
    }

    #[sqlx::test(migrations = false)]
    async fn the_counts_of_usage_are_made_right_again_by_0017(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        sqlx::query(
            "INSERT INTO module_kv (module, guild_id, key, value, revision)
             VALUES ('warn', 1, 'a', '1'::jsonb, 1), ('warn', 1, 'b', '1'::jsonb, 2)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE module_kv_usage SET entries = 99 WHERE module = 'warn'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO message_file_usage VALUES (7, 1234)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0017_recount_usage.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let entries: i64 = sqlx::query_scalar(
            "SELECT entries FROM module_kv_usage WHERE module = 'warn' AND guild_id = 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(entries, 2);
        let bytes: i64 =
            sqlx::query_scalar("SELECT bytes FROM message_file_usage WHERE guild_id = 7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(bytes, 0, "a guild with no stored files has none counted");
    }
}
