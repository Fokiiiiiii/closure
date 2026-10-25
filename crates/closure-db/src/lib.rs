//! PostgreSQL persistence.
//!
//! The database is the source of truth for guild configuration. Every write is one transaction
//! that also appends history and notifies other processes, configuration edits use optimistic
//! concurrency (a stale editor gets a conflict instead of silently overwriting), and queries are
//! parameterised and exercised against a real database in the tests.

mod audit;
mod config;
mod error;
mod guilds;
mod ids;
mod scheduler;
mod storage;

use std::str::FromStr;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub use audit::{AuditEvent, AuditRecord};
pub use config::{CONFIG_CHANNEL, ConfigEvent, ConfigHistoryEntry, ConfigListener, StoredConfig};
pub use error::DbError;

/// Connection pool settings. The defaults suit a single bot process; raise `max_connections`
/// only after checking the server's connection limit.
#[derive(Debug, Clone)]
pub struct DbOptions {
    pub max_connections: u32,
    /// How long a caller waits for a free connection before the call fails.
    pub acquire_timeout: Duration,
    /// Server-side cap on a single statement, so one bad query cannot hold a connection.
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
                "audit_events",
                "guild_config",
                "guild_config_history",
                "guilds",
                "module_counters",
                "module_kv",
                "scheduled_jobs"
            ]
        );
    }
}
