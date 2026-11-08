//! Entries and counters of modules (see `closure_core::storage`).

use std::time::Duration;

use async_trait::async_trait;
use closure_core::event::GuildId;
use closure_core::module::ModuleId;
use closure_core::storage::{
    Entry, Expect, MAX_COUNTERS, MAX_ENTRIES, MAX_WINDOW, StorageBackend, StorageError,
    window_minutes,
};
use sqlx::{Postgres, Transaction};

use crate::Db;
use crate::error::DbError;
use crate::ids::to_db;

fn backend(error: impl std::fmt::Display) -> StorageError {
    StorageError::Backend(error.to_string())
}

fn guild_id(guild: GuildId) -> Result<i64, StorageError> {
    to_db(guild).map_err(backend)
}

fn subject_id(subject: u64) -> Result<i64, StorageError> {
    i64::try_from(subject)
        .map_err(|_| StorageError::InvalidCounter("the subject does not fit 63 bits".into()))
}

/// How many minute rows one purge statement deletes, and how long a purge keeps going.
const PURGE_BATCH: i64 = 10_000;
const PURGE_BUDGET: Duration = Duration::from_secs(5);

/// Adds to a counter row (the total is bucket 0), making it if there is none.
const ADD_TO_COUNTER: &str =
    "INSERT INTO module_counters (module, guild_id, name, subject, bucket, value)
     VALUES ($1, $2, $3, $4, $5, $6)
     ON CONFLICT (module, guild_id, name, subject, bucket)
     DO UPDATE SET value = module_counters.value + EXCLUDED.value
     RETURNING value";

/// Writes of one module in one guild take turns, so the quotas are exact.
async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    module: &ModuleId,
    guild: i64,
) -> Result<(), StorageError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || ':' || $2::text, 0))")
        .bind(module.as_str())
        .bind(guild)
        .execute(&mut **tx)
        .await
        .map_err(backend)?;
    Ok(())
}

/// One key's writes take turns.
async fn lock_key(
    tx: &mut Transaction<'_, Postgres>,
    module: &ModuleId,
    guild: i64,
    key: &str,
) -> Result<(), StorageError> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended($1 || ':' || $2::text || ':' || $3, 1))",
    )
    .bind(module.as_str())
    .bind(guild)
    .bind(key)
    .execute(&mut **tx)
    .await
    .map_err(backend)?;
    Ok(())
}

#[async_trait]
impl StorageBackend for Db {
    async fn get(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<Option<Entry>, StorageError> {
        let row: Option<(serde_json::Value, i64)> = sqlx::query_as(
            "SELECT value, revision FROM module_kv
             WHERE module = $1 AND guild_id = $2 AND key = $3
               AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.map(|(value, revision)| Entry { value, revision }))
    }

    async fn put(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
        value: serde_json::Value,
        ttl: Option<Duration>,
        expect: Expect,
    ) -> Result<i64, StorageError> {
        let guild = guild_id(guild)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_key(&mut tx, module, guild, key).await?;
        // An expired entry is as good as none.
        sqlx::query(
            "DELETE FROM module_kv
             WHERE module = $1 AND guild_id = $2 AND key = $3 AND expires_at <= now()",
        )
        .bind(module.as_str())
        .bind(guild)
        .bind(key)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM module_kv WHERE module = $1 AND guild_id = $2 AND key = $3",
        )
        .bind(module.as_str())
        .bind(guild)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        match expect {
            Expect::Any => {}
            Expect::Missing if current.is_none() => {}
            Expect::Revision(wanted) if current == Some(wanted) => {}
            _ => return Err(StorageError::Conflict),
        }
        if current.is_none() {
            // New keys take turns, so the count is exact. It is read from the row the table keeps
            // for the module and the guild, so a new entry costs the same however many there are.
            lock(&mut tx, module, guild).await?;
            let usage = "SELECT COALESCE(
                             (SELECT entries FROM module_kv_usage
                              WHERE module = $1 AND guild_id = $2),
                             0)";
            let mut kept: i64 = sqlx::query_scalar(usage)
                .bind(module.as_str())
                .bind(guild)
                .fetch_one(&mut *tx)
                .await
                .map_err(backend)?;
            if usize::try_from(kept).unwrap_or(usize::MAX) >= MAX_ENTRIES {
                // The entries that ran out but are not purged yet take room: they go first.
                sqlx::query(
                    "DELETE FROM module_kv WHERE ctid IN (
                         SELECT ctid FROM module_kv
                         WHERE module = $1 AND guild_id = $2 AND expires_at <= now()
                         LIMIT 1000)",
                )
                .bind(module.as_str())
                .bind(guild)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
                kept = sqlx::query_scalar(usage)
                    .bind(module.as_str())
                    .bind(guild)
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(backend)?;
            }
            if usize::try_from(kept).unwrap_or(usize::MAX) >= MAX_ENTRIES {
                return Err(StorageError::QuotaExceeded { limit: MAX_ENTRIES });
            }
        }
        let seconds = ttl.map(|ttl| ttl.as_secs_f64().max(1.0));
        let revision: i64 = sqlx::query_scalar(
            "INSERT INTO module_kv (module, guild_id, key, value, revision, expires_at)
             VALUES ($1, $2, $3, $4, nextval('module_kv_revisions'),
                     CASE WHEN $5::float8 IS NULL THEN NULL
                          ELSE now() + make_interval(secs => $5::float8) END)
             ON CONFLICT (module, guild_id, key) DO UPDATE
                SET value = EXCLUDED.value,
                    revision = EXCLUDED.revision,
                    expires_at = EXCLUDED.expires_at,
                    updated_at = now()
             RETURNING revision",
        )
        .bind(module.as_str())
        .bind(guild)
        .bind(key)
        .bind(value)
        .bind(seconds)
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(revision)
    }

    async fn delete(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<bool, StorageError> {
        // Takes its turn with the writes of the key. A new key being counted at the same time sees
        // one entry more than there is, which only makes the quota stricter for that moment.
        let guild = guild_id(guild)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock_key(&mut tx, module, guild, key).await?;
        let live: Option<bool> = sqlx::query_scalar(
            "DELETE FROM module_kv WHERE module = $1 AND guild_id = $2 AND key = $3
             RETURNING (expires_at IS NULL OR expires_at > now())",
        )
        .bind(module.as_str())
        .bind(guild)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(live.unwrap_or(false))
    }

    async fn list(
        &self,
        module: &ModuleId,
        guild: GuildId,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Entry)>, StorageError> {
        let rows: Vec<(String, serde_json::Value, i64)> = sqlx::query_as(
            "SELECT key, value, revision FROM module_kv
             WHERE module = $1 AND guild_id = $2 AND starts_with(key, $3)
               AND (expires_at IS NULL OR expires_at > now())
             ORDER BY key LIMIT $4",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(prefix)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(rows
            .into_iter()
            .map(|(key, value, revision)| (key, Entry { value, revision }))
            .collect())
    }

    async fn counter_add(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
        delta: i64,
    ) -> Result<i64, StorageError> {
        let guild = guild_id(guild)?;
        let subject = subject_id(subject)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock(&mut tx, module, guild).await?;
        let known: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM module_counters
                            WHERE module = $1 AND guild_id = $2 AND name = $3
                              AND subject = $4 AND bucket = 0)",
        )
        .bind(module.as_str())
        .bind(guild)
        .bind(counter)
        .bind(subject)
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        if !known {
            let kept: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM (
                     SELECT 1 FROM module_counters
                     WHERE module = $1 AND guild_id = $2 AND bucket = 0
                     LIMIT $3) AS counted",
            )
            .bind(module.as_str())
            .bind(guild)
            .bind(i64::try_from(MAX_COUNTERS).unwrap_or(i64::MAX))
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
            if usize::try_from(kept).unwrap_or(usize::MAX) >= MAX_COUNTERS {
                return Err(StorageError::QuotaExceeded {
                    limit: MAX_COUNTERS,
                });
            }
        }
        let total: i64 = sqlx::query_scalar(ADD_TO_COUNTER)
            .bind(module.as_str())
            .bind(guild)
            .bind(counter)
            .bind(subject)
            .bind(0_i64)
            .bind(delta)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
        let minute: i64 = sqlx::query_scalar("SELECT (extract(epoch FROM now())::bigint / 60)")
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
        sqlx::query_scalar::<_, i64>(ADD_TO_COUNTER)
            .bind(module.as_str())
            .bind(guild)
            .bind(counter)
            .bind(subject)
            .bind(minute)
            .bind(delta)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(total)
    }

    async fn counter_get(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
        window: Option<Duration>,
    ) -> Result<i64, StorageError> {
        let guild = guild_id(guild)?;
        let subject = subject_id(subject)?;
        let value: Option<i64> = match window {
            None => sqlx::query_scalar(
                "SELECT value FROM module_counters
                 WHERE module = $1 AND guild_id = $2 AND name = $3 AND subject = $4 AND bucket = 0",
            )
            .bind(module.as_str())
            .bind(guild)
            .bind(counter)
            .bind(subject)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?,
            Some(window) => sqlx::query_scalar(
                "SELECT COALESCE(sum(value), 0)::bigint FROM module_counters
                 WHERE module = $1 AND guild_id = $2 AND name = $3 AND subject = $4
                   AND bucket > 0 AND bucket >= (extract(epoch FROM now())::bigint / 60) - $5 + 1",
            )
            .bind(module.as_str())
            .bind(guild)
            .bind(counter)
            .bind(subject)
            .bind(window_minutes(window))
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?,
        };
        Ok(value.unwrap_or(0))
    }

    async fn counter_reset(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
    ) -> Result<(), StorageError> {
        // Takes its turn with the additions: one in progress adds its minute row before this
        // deletes, so the total and the minutes are never left disagreeing.
        let guild = guild_id(guild)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        lock(&mut tx, module, guild).await?;
        sqlx::query(
            "DELETE FROM module_counters
             WHERE module = $1 AND guild_id = $2 AND name = $3 AND subject = $4",
        )
        .bind(module.as_str())
        .bind(guild)
        .bind(counter)
        .bind(subject_id(subject)?)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        Ok(())
    }

    async fn score_add(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        subject: u64,
        delta: i64,
    ) -> Result<i64, StorageError> {
        sqlx::query_scalar(
            "INSERT INTO module_scores (module, guild_id, name, subject, value)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (module, guild_id, name, subject)
             DO UPDATE SET value = module_scores.value + EXCLUDED.value
             RETURNING value",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(subject_id(subject)?)
        .bind(delta)
        .fetch_one(&self.pool)
        .await
        .map_err(backend)
    }

    async fn score_get(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        subject: u64,
    ) -> Result<i64, StorageError> {
        let value: Option<i64> = sqlx::query_scalar(
            "SELECT value FROM module_scores
             WHERE module = $1 AND guild_id = $2 AND name = $3 AND subject = $4",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(subject_id(subject)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(value.unwrap_or(0))
    }

    async fn score_put(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        subject: u64,
        value: i64,
    ) -> Result<(), StorageError> {
        let (guild, subject) = (guild_id(guild)?, subject_id(subject)?);
        if value == 0 {
            sqlx::query(
                "DELETE FROM module_scores
                 WHERE module = $1 AND guild_id = $2 AND name = $3 AND subject = $4",
            )
            .bind(module.as_str())
            .bind(guild)
            .bind(name)
            .bind(subject)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        } else {
            sqlx::query(
                "INSERT INTO module_scores (module, guild_id, name, subject, value)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (module, guild_id, name, subject)
                 DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(module.as_str())
            .bind(guild)
            .bind(name)
            .bind(subject)
            .bind(value)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        }
        Ok(())
    }

    async fn score_top(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        limit: usize,
    ) -> Result<Vec<(u64, i64)>, StorageError> {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT subject, value FROM module_scores
             WHERE module = $1 AND guild_id = $2 AND name = $3
             ORDER BY value DESC, subject LIMIT $4",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(rows
            .into_iter()
            .filter_map(|(subject, value)| Some((u64::try_from(subject).ok()?, value)))
            .collect())
    }

    async fn score_above(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        value: i64,
    ) -> Result<u64, StorageError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM module_scores
             WHERE module = $1 AND guild_id = $2 AND name = $3 AND value > $4",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(value)
        .fetch_one(&self.pool)
        .await
        .map_err(backend)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    async fn score_count(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
    ) -> Result<u64, StorageError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM module_scores
             WHERE module = $1 AND guild_id = $2 AND name = $3",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(backend)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    async fn set_add(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        member: u64,
    ) -> Result<bool, StorageError> {
        let done = sqlx::query(
            "INSERT INTO module_sets (module, guild_id, name, member)
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(subject_id(member)?)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(done.rows_affected() == 1)
    }

    async fn set_remove(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        member: u64,
    ) -> Result<bool, StorageError> {
        let done = sqlx::query(
            "DELETE FROM module_sets
             WHERE module = $1 AND guild_id = $2 AND name = $3 AND member = $4",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(subject_id(member)?)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(done.rows_affected() == 1)
    }

    async fn set_has(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        member: u64,
    ) -> Result<bool, StorageError> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM module_sets
                            WHERE module = $1 AND guild_id = $2 AND name = $3 AND member = $4)",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(subject_id(member)?)
        .fetch_one(&self.pool)
        .await
        .map_err(backend)
    }

    async fn set_count(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
    ) -> Result<u64, StorageError> {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM module_sets
             WHERE module = $1 AND guild_id = $2 AND name = $3",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(backend)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    async fn set_after(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        after: u64,
        count: usize,
    ) -> Result<Vec<u64>, StorageError> {
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT member FROM module_sets
             WHERE module = $1 AND guild_id = $2 AND name = $3 AND member > $4
             ORDER BY member LIMIT $5",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(i64::try_from(after).unwrap_or(i64::MAX))
        .bind(i64::try_from(count).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(rows
            .into_iter()
            .filter_map(|m| u64::try_from(m).ok())
            .collect())
    }

    async fn set_sample(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        count: usize,
    ) -> Result<Vec<u64>, StorageError> {
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT member FROM module_sets
             WHERE module = $1 AND guild_id = $2 AND name = $3
             ORDER BY random() LIMIT $4",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(i64::try_from(count).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(rows
            .into_iter()
            .filter_map(|m| u64::try_from(m).ok())
            .collect())
    }

    async fn set_take(
        &self,
        module: &ModuleId,
        guild: GuildId,
        name: &str,
        count: usize,
    ) -> Result<Vec<u64>, StorageError> {
        // `SKIP LOCKED` hands two callers at once different members.
        let rows: Vec<i64> = sqlx::query_scalar(
            "DELETE FROM module_sets WHERE ctid IN (
                 SELECT ctid FROM module_sets
                 WHERE module = $1 AND guild_id = $2 AND name = $3
                 LIMIT $4 FOR UPDATE SKIP LOCKED)
             RETURNING member",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(name)
        .bind(i64::try_from(count).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        Ok(rows
            .into_iter()
            .filter_map(|m| u64::try_from(m).ok())
            .collect())
    }
}

impl Db {
    /// Deletes entries that have expired and the per-minute history older than the widest window
    /// a module may ask for. Returns how many of each were deleted. Run it now and then.
    pub async fn purge_module_storage(&self) -> Result<(u64, u64), DbError> {
        let entries = sqlx::query("DELETE FROM module_kv WHERE expires_at <= now()")
            .execute(&self.pool)
            .await?
            .rows_affected();
        let minutes = self.purge_counter_minutes().await?;
        Ok((entries, minutes))
    }

    /// Deletes the per-minute history past the widest window in batches, for a bounded time: a backlog of
    /// any size is worked off over several runs instead of in one statement that the timeout would cancel
    /// and the next run would start over.
    async fn purge_counter_minutes(&self) -> Result<u64, DbError> {
        let started = std::time::Instant::now();
        let mut deleted = 0;
        while started.elapsed() < PURGE_BUDGET {
            let batch = sqlx::query(
                "DELETE FROM module_counters WHERE ctid IN (
                     SELECT ctid FROM module_counters
                     WHERE bucket > 0 AND bucket < (extract(epoch FROM now())::bigint / 60) - $1
                     LIMIT $2)",
            )
            .bind(window_minutes(MAX_WINDOW))
            .bind(PURGE_BATCH)
            .execute(&self.pool)
            .await?
            .rows_affected();
            deleted += batch;
            if batch < u64::try_from(PURGE_BATCH).unwrap_or(u64::MAX) {
                break;
            }
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::capability::{Capability, CapabilitySet};
    use closure_core::storage::{MemoryStorage, Store};
    use serde_json::json;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    fn store(db: &Db, module: &str, guild: u64) -> Store {
        let granted: CapabilitySet = [Capability::Storage].into_iter().collect();
        Store::new(
            Arc::new(db.clone()) as Arc<dyn StorageBackend>,
            &granted,
            ModuleId::new(module).unwrap(),
            Id::new(guild),
        )
    }

    #[sqlx::test]
    async fn entries_round_trip_per_module_and_guild(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let (warn, other, elsewhere) = (
            store(&db, "warn", 1),
            store(&db, "mute", 1),
            store(&db, "warn", 2),
        );
        warn.set("a", &json!({ "n": 1 })).await.unwrap();
        assert_eq!(
            warn.get::<serde_json::Value>("a").await.unwrap(),
            Some(json!({ "n": 1 }))
        );
        assert_eq!(other.get::<serde_json::Value>("a").await.unwrap(), None);
        assert_eq!(elsewhere.get::<serde_json::Value>("a").await.unwrap(), None);
        assert!(warn.delete("a").await.unwrap());
        assert!(!warn.delete("a").await.unwrap());
        for key in ["case:2", "case:1", "case%x", "cased"] {
            warn.set(key, &key).await.unwrap();
        }
        let keys: Vec<String> = warn
            .list("case:", 10)
            .await
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, ["case:1", "case:2"]);
        assert_eq!(warn.list("case%", 10).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    async fn revisions_make_a_lost_update_a_conflict_and_ttl_hides_entries(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let store = store(&db, "warn", 1);
        assert_eq!(
            store.replace("n", &1, Expect::Missing, None).await.unwrap(),
            1
        );
        assert_eq!(
            store.replace("n", &2, Expect::Missing, None).await,
            Err(StorageError::Conflict)
        );
        assert_eq!(
            store
                .replace("n", &2, Expect::Revision(1), None)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store.replace("n", &3, Expect::Revision(1), None).await,
            Err(StorageError::Conflict)
        );
        assert_eq!(store.get::<i64>("n").await.unwrap(), Some(2));

        store
            .set_for("short", &1, Duration::from_secs(1))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE module_kv SET expires_at = now() - interval '1 second' WHERE key = 'short'",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(store.get::<i64>("short").await.unwrap(), None);
        assert!(
            store
                .list("", 10)
                .await
                .unwrap()
                .iter()
                .all(|(key, _)| key != "short")
        );
        assert_eq!(
            store
                .replace("short", &5, Expect::Missing, None)
                .await
                .unwrap(),
            4
        );
        sqlx::query(
            "UPDATE module_kv SET expires_at = now() - interval '1 second' WHERE key = 'short'",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(db.purge_module_storage().await.unwrap().0, 1);
    }

    #[sqlx::test]
    async fn a_revision_read_before_a_delete_never_matches_the_entry_made_again(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let store = store(&db, "warn", 1);
        store.set("n", &"first").await.unwrap();
        let old = store.entry("n").await.unwrap().unwrap().revision;
        store.delete("n").await.unwrap();
        store.set("n", &"second").await.unwrap();
        assert_eq!(
            store
                .replace("n", &"stale", Expect::Revision(old), None)
                .await,
            Err(StorageError::Conflict)
        );
        assert_eq!(
            store.get::<String>("n").await.unwrap().as_deref(),
            Some("second")
        );
    }

    async fn waits_for_the_writers_turn<F, T>(db: &Db, key: Option<&str>, call: F)
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let module = ModuleId::new("warn").unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        match key {
            Some(key) => lock_key(&mut tx, &module, 1, key).await.unwrap(),
            None => lock(&mut tx, &module, 1).await.unwrap(),
        }
        let task = tokio::spawn(call);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!task.is_finished(), "it did not wait for the writer");
        tx.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("it finishes once the writer is done")
            .unwrap();
    }

    #[sqlx::test]
    async fn deleting_and_resetting_take_their_turn_with_the_writers(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let store = store(&db, "warn", 1);
        store.set("n", &1).await.unwrap();
        store.counter_add("c", 1_u64, 1).await.unwrap();
        let (deleter, resetter) = (store.clone(), store.clone());
        waits_for_the_writers_turn(
            &db,
            Some("n"),
            async move { deleter.delete("n").await.unwrap() },
        )
        .await;
        waits_for_the_writers_turn(&db, None, async move {
            resetter.counter_reset("c", 1_u64).await.unwrap();
        })
        .await;
        assert_eq!(store.get::<i64>("n").await.unwrap(), None);
        assert_eq!(store.counter_total("c", 1_u64).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn an_entry_is_updated_while_the_module_counts_a_new_key(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let store = store(&db, "warn", 1);
        store.set("n", &1).await.unwrap();
        let module = ModuleId::new("warn").unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        lock(&mut tx, &module, 1).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), store.set("n", &2))
            .await
            .expect("an update does not wait for the module's turn")
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(store.get::<i64>("n").await.unwrap(), Some(2));
    }

    #[sqlx::test]
    async fn concurrent_compare_and_set_lets_exactly_one_writer_win(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let store = store(&db, "warn", 1);
        store.set("n", &0).await.unwrap();
        let attempts: Vec<_> = (0..8)
            .map(|n| {
                let store = store.clone();
                tokio::spawn(async move { store.replace("n", &n, Expect::Revision(1), None).await })
            })
            .collect();
        let mut won = 0;
        for attempt in attempts {
            if attempt.await.unwrap().is_ok() {
                won += 1;
            }
        }
        assert_eq!(won, 1);
    }

    #[sqlx::test]
    async fn legacy_restriction_lists_are_moved_into_the_shared_one(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        sqlx::query(
            "INSERT INTO module_kv (module, guild_id, key, value, revision, expires_at)
             VALUES ('moderation', 1, 'stripped:7', '[800, 801]', 1, NULL),
                    ('moderation', 1, 'stripped:7:vcmute', '[801, 802]', 2, NULL),
                    ('automod', 1, 'stripped:8:timeout', '[803]', 3, now() + interval '1 day'),
                    ('restrictions', 1, 'stripped:8', '[805]', 4, now() + interval '2 days'),
                    ('automod', 1, 'stripped:9', '[804]', 5, now() - interval '1 day')",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!("../migrations/0009_restrictions_shared.sql"))
            .execute(&db.pool)
            .await
            .unwrap();

        let shared: Vec<(String, serde_json::Value, bool)> = sqlx::query_as(
            "SELECT key, value, expires_at IS NULL FROM module_kv
             WHERE module = 'restrictions' ORDER BY key",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            shared,
            [
                ("stripped:7".to_owned(), json!([800, 801, 802]), true),
                ("stripped:8".to_owned(), json!([803, 805]), false),
            ]
        );
        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM module_kv
             WHERE module <> 'restrictions' AND key LIKE 'stripped:%'",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(left, 0, "the old lists are removed");
    }

    #[sqlx::test]
    async fn the_quota_of_entries_is_exact_under_concurrency(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        sqlx::query(
            "INSERT INTO module_kv (module, guild_id, key, value, revision)
             SELECT 'warn', 1, 'k' || n, '1'::jsonb, 1 FROM generate_series(1, 5) AS n",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE module_kv_usage SET entries = $1 WHERE module = 'warn' AND guild_id = 1",
        )
        .bind(i64::try_from(MAX_ENTRIES - 3).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
        let store = store(&db, "warn", 1);
        let attempts: Vec<_> = (0..8)
            .map(|n| {
                let store = store.clone();
                tokio::spawn(async move { store.set(&format!("new{n}"), &n).await })
            })
            .collect();
        let mut stored = 0;
        for attempt in attempts {
            match attempt.await.unwrap() {
                Ok(()) => stored += 1,
                Err(error) => assert_eq!(error, StorageError::QuotaExceeded { limit: MAX_ENTRIES }),
            }
        }
        assert_eq!(stored, 3);
    }

    #[sqlx::test]
    async fn the_count_of_entries_follows_writes_deletes_and_the_purge(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let store = store(&db, "warn", 1);
        let used = |db: Db| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COALESCE((SELECT entries FROM module_kv_usage
                                  WHERE module = 'warn' AND guild_id = 1), -1)",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap()
        };
        assert_eq!(used(db.clone()).await, -1);
        store.set("a", &1).await.unwrap();
        store.set("b", &1).await.unwrap();
        store.set("a", &2).await.unwrap();
        assert_eq!(
            used(db.clone()).await,
            2,
            "a replaced entry is not counted twice"
        );
        store.delete("b").await.unwrap();
        assert_eq!(used(db.clone()).await, 1);
        store
            .set_for("short", &1, Duration::from_secs(1))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE module_kv SET expires_at = now() - interval '1 second' WHERE key = 'short'",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        db.purge_module_storage().await.unwrap();
        assert_eq!(
            used(db.clone()).await,
            1,
            "what the purge removed is not counted"
        );
    }

    #[sqlx::test]
    async fn counters_keep_a_total_and_the_last_minutes(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let store = store(&db, "warn", 1);
        assert_eq!(store.counter_total("warns", 7_u64).await.unwrap(), 0);
        assert_eq!(store.counter_add("warns", 7_u64, 2).await.unwrap(), 2);
        assert_eq!(store.counter_add("warns", 7_u64, 3).await.unwrap(), 5);
        assert_eq!(store.counter_total("warns", 7_u64).await.unwrap(), 5);
        assert_eq!(
            store
                .counter_recent("warns", 7_u64, Duration::from_secs(60))
                .await
                .unwrap(),
            5
        );
        sqlx::query("UPDATE module_counters SET bucket = bucket - 60 WHERE bucket > 0")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            store
                .counter_recent("warns", 7_u64, Duration::from_mins(30))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .counter_recent("warns", 7_u64, Duration::from_mins(61))
                .await
                .unwrap(),
            5
        );
        sqlx::query("UPDATE module_counters SET bucket = bucket - 600000 WHERE bucket > 0")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(db.purge_module_storage().await.unwrap().1, 1);
        assert_eq!(store.counter_total("warns", 7_u64).await.unwrap(), 5);
        assert_eq!(store.counter_total("warns", 8_u64).await.unwrap(), 0);
        assert_eq!(
            self::store(&db, "mute", 1)
                .counter_total("warns", 7_u64)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            self::store(&db, "warn", 2)
                .counter_total("warns", 7_u64)
                .await
                .unwrap(),
            0
        );
        store.counter_reset("warns", 7_u64).await.unwrap();
        assert_eq!(store.counter_total("warns", 7_u64).await.unwrap(), 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM module_counters")
                .fetch_one(&db.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[sqlx::test]
    async fn a_backlog_of_old_minutes_is_purged_in_batches(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let rows = i64::from(2 * 10_000 + 5);
        sqlx::query(
            "INSERT INTO module_counters (module, guild_id, name, subject, bucket, value)
             SELECT 'warn', 1, 'old', 1, g, 1 FROM generate_series(1, $1) AS g",
        )
        .bind(rows)
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(db.purge_module_storage().await.unwrap().1, 20_005);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM module_counters")
                .fetch_one(&db.pool)
                .await
                .unwrap(),
            0
        );
    }

    #[sqlx::test]
    async fn concurrent_additions_are_not_lost(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let store = store(&db, "warn", 1);
        let adds: Vec<_> = (0..20)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move { store.counter_add("n", 1_u64, 1).await })
            })
            .collect();
        for add in adds {
            add.await.unwrap().unwrap();
        }
        assert_eq!(store.counter_total("n", 1_u64).await.unwrap(), 20);
        assert_eq!(
            store
                .counter_recent("n", 1_u64, Duration::from_secs(120))
                .await
                .unwrap(),
            20
        );
    }

    #[sqlx::test]
    async fn the_database_and_the_memory_backend_agree(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let memory = Arc::new(MemoryStorage::default());
        let granted: CapabilitySet = [Capability::Storage].into_iter().collect();
        let in_memory = Store::new(
            memory as Arc<dyn StorageBackend>,
            &granted,
            ModuleId::new("warn").unwrap(),
            Id::new(1),
        );
        let in_db = store(&db, "warn", 1);
        for store in [&in_memory, &in_db] {
            assert_eq!(
                store.replace("k", &1, Expect::Missing, None).await.unwrap(),
                1
            );
            assert_eq!(
                store
                    .replace("k", &2, Expect::Revision(1), None)
                    .await
                    .unwrap(),
                2
            );
            assert_eq!(
                store.replace("k", &3, Expect::Revision(1), None).await,
                Err(StorageError::Conflict)
            );
            assert_eq!(store.counter_add("c", 5_u64, 4).await.unwrap(), 4);
            assert_eq!(store.counter_add("c", 5_u64, -1).await.unwrap(), 3);
            assert_eq!(
                store
                    .counter_recent("c", 5_u64, Duration::from_secs(60))
                    .await
                    .unwrap(),
                3
            );
            assert_eq!(store.list("", 5).await.unwrap().len(), 1);
            assert!(store.delete("k").await.unwrap());
        }
    }

    #[sqlx::test]
    async fn scores_rank_and_sets_draw_the_same_in_the_database_and_in_memory(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.migrate().await.unwrap();
        let memory = Arc::new(MemoryStorage::default());
        let granted: CapabilitySet = [Capability::Storage].into_iter().collect();
        let in_memory = Store::new(
            memory as Arc<dyn StorageBackend>,
            &granted,
            ModuleId::new("leveling").unwrap(),
            Id::new(1),
        );
        let in_db = store(&db, "leveling", 1);
        for store in [&in_memory, &in_db] {
            assert_eq!(store.score_add("xp", 7_u64, 30).await.unwrap(), 30);
            assert_eq!(store.score_add("xp", 7_u64, 20).await.unwrap(), 50);
            store.score_add("xp", 8_u64, 50).await.unwrap();
            store.score_add("xp", 9_u64, 10).await.unwrap();
            assert_eq!(store.score("xp", 7_u64).await.unwrap(), 50);
            assert_eq!(store.score("xp", 99_u64).await.unwrap(), 0);
            assert_eq!(store.score_top("xp", 2).await.unwrap(), [(7, 50), (8, 50)]);
            assert_eq!(store.score_above("xp", 50).await.unwrap(), 0);
            assert_eq!(store.score_above("xp", 10).await.unwrap(), 2);
            assert_eq!(store.score_count("xp").await.unwrap(), 3);
            store.score_put("xp", 8_u64, 0).await.unwrap();
            assert_eq!(store.score_count("xp").await.unwrap(), 2);
            store.score_put("xp", 9_u64, 70).await.unwrap();
            assert_eq!(store.score_top("xp", 1).await.unwrap(), [(9, 70)]);
            assert_eq!(store.score_count("other").await.unwrap(), 0);

            assert!(store.set_add("giveaway", 1_u64).await.unwrap());
            assert!(!store.set_add("giveaway", 1_u64).await.unwrap());
            for member in 2..=20_u64 {
                store.set_add("giveaway", member).await.unwrap();
            }
            assert_eq!(store.set_count("giveaway").await.unwrap(), 20);
            assert!(store.set_has("giveaway", 5_u64).await.unwrap());
            assert!(store.set_remove("giveaway", 5_u64).await.unwrap());
            assert!(!store.set_remove("giveaway", 5_u64).await.unwrap());
            assert!(!store.set_has("giveaway", 5_u64).await.unwrap());
            let mut drawn = store.set_sample("giveaway", 8).await.unwrap();
            assert_eq!(drawn.len(), 8);
            assert!(!drawn.contains(&5));
            drawn.sort_unstable();
            drawn.dedup();
            assert_eq!(drawn.len(), 8, "each member once");
            assert_eq!(store.set_sample("giveaway", 100).await.unwrap().len(), 19);
            assert_eq!(
                store.set_after("giveaway", 0_u64, 3).await.unwrap(),
                [1, 2, 3]
            );
            assert_eq!(
                store.set_after("giveaway", 3_u64, 3).await.unwrap(),
                [4, 6, 7]
            );
            assert!(
                store
                    .set_after("giveaway", 20_u64, 3)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(store.set_count("giveaway").await.unwrap(), 19);
            let mut first = store.set_take("giveaway", 5).await.unwrap();
            let rest = store.set_take("giveaway", 100).await.unwrap();
            assert_eq!((first.len(), rest.len()), (5, 14));
            first.extend(rest);
            first.sort_unstable();
            first.dedup();
            assert_eq!(first.len(), 19, "nobody twice");
            assert_eq!(store.set_count("giveaway").await.unwrap(), 0);
            assert!(store.set_take("giveaway", 5).await.unwrap().is_empty());
        }
        let elsewhere = store(&db, "leveling", 2);
        assert_eq!(elsewhere.score_count("xp").await.unwrap(), 0);
        assert_eq!(elsewhere.set_count("giveaway").await.unwrap(), 0);
    }
}
