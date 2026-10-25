//! Jobs of modules (see `closure_core::schedule`).

use std::time::Duration;

use async_trait::async_trait;
use closure_core::event::GuildId;
use closure_core::module::ModuleId;
use closure_core::schedule::{
    MAX_ATTEMPTS, MAX_JOBS, NewJob, RunAt, ScheduleError, ScheduledJob, SchedulerBackend,
};

use crate::Db;
use crate::error::DbError;
use crate::ids::{from_db, to_db};

fn backend(error: impl std::fmt::Display) -> ScheduleError {
    ScheduleError::Backend(error.to_string())
}

fn guild_id(guild: GuildId) -> Result<i64, ScheduleError> {
    to_db(guild).map_err(backend)
}

fn seconds(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

type Claimed = (i64, i64, String, String, serde_json::Value, i32, i64);

fn job_of(
    (id, guild, module, name, payload, attempts, token): Claimed,
) -> Result<ScheduledJob, ScheduleError> {
    Ok(ScheduledJob {
        id,
        guild_id: from_db(guild).map_err(backend)?,
        module: ModuleId::new(&module).map_err(backend)?,
        name,
        payload,
        attempt: u32::try_from(attempts).unwrap_or(u32::MAX),
        token,
    })
}

#[async_trait]
impl SchedulerBackend for Db {
    async fn schedule(&self, job: NewJob) -> Result<i64, ScheduleError> {
        let guild = guild_id(job.guild)?;
        let (after, at) = match job.run_at {
            RunAt::After(delay) => (Some(seconds(delay)), None),
            // Unix seconds up to the year 9999 are exact in a double.
            #[allow(clippy::cast_precision_loss)]
            RunAt::At(at) => (None, Some(at as f64)),
        };
        let every = job
            .every
            .map(|every| i64::try_from(every.as_secs()).unwrap_or(i64::MAX));
        let mut tx = self.pool.begin().await.map_err(backend)?;
        // Jobs of one module in one guild are added one at a time, so the quota is exact.
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('job:' || $1 || ':' || $2::text, 0))",
        )
        .bind(job.module.as_str())
        .bind(guild)
        .execute(&mut *tx)
        .await
        .map_err(backend)?;
        let existing: Option<i64> = match &job.key {
            Some(key) => sqlx::query_scalar(
                "SELECT id FROM scheduled_jobs
                 WHERE module = $1 AND guild_id = $2 AND dedupe_key = $3",
            )
            .bind(job.module.as_str())
            .bind(guild)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(backend)?,
            None => None,
        };
        let id = if let Some(id) = existing {
            sqlx::query(
                "UPDATE scheduled_jobs
                 SET name = $2, payload = $3, every_seconds = $4, attempts = 0,
                     leased_until = NULL, state = 'pending', last_error = NULL, updated_at = now(),
                     claim_token = 0,
                     run_at = COALESCE(to_timestamp($5), now() + $6 * interval '1 second'),
                     planned_at = COALESCE(to_timestamp($5), now() + $6 * interval '1 second')
                 WHERE id = $1",
            )
            .bind(id)
            .bind(&job.name)
            .bind(&job.payload)
            .bind(every)
            .bind(at)
            .bind(after)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
            id
        } else {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM scheduled_jobs WHERE module = $1 AND guild_id = $2",
            )
            .bind(job.module.as_str())
            .bind(guild)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
            if usize::try_from(waiting).unwrap_or(usize::MAX) >= MAX_JOBS {
                return Err(ScheduleError::QuotaExceeded { limit: MAX_JOBS });
            }
            sqlx::query_scalar(
                "INSERT INTO scheduled_jobs
                     (module, guild_id, name, payload, every_seconds, dedupe_key, run_at, planned_at)
                 VALUES ($1, $2, $3, $4, $5, $6,
                         COALESCE(to_timestamp($7), now() + $8 * interval '1 second'),
                         COALESCE(to_timestamp($7), now() + $8 * interval '1 second'))
                 RETURNING id",
            )
            .bind(job.module.as_str())
            .bind(guild)
            .bind(&job.name)
            .bind(&job.payload)
            .bind(every)
            .bind(&job.key)
            .bind(at)
            .bind(after)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?
        };
        tx.commit().await.map_err(backend)?;
        Ok(id)
    }

    async fn cancel(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<bool, ScheduleError> {
        let removed = sqlx::query(
            "DELETE FROM scheduled_jobs WHERE module = $1 AND guild_id = $2 AND dedupe_key = $3",
        )
        .bind(module.as_str())
        .bind(guild_id(guild)?)
        .bind(key)
        .execute(&self.pool)
        .await
        .map_err(backend)?
        .rows_affected();
        Ok(removed > 0)
    }

    async fn claim_due(
        &self,
        limit: usize,
        lease: Duration,
    ) -> Result<Vec<ScheduledJob>, ScheduleError> {
        // A job taken over and over without ever being reported is given up on.
        sqlx::query(
            "UPDATE scheduled_jobs
             SET state = 'failed', last_error = 'taken too many times without an answer',
                 updated_at = now()
             WHERE state = 'pending' AND attempts >= $1 AND leased_until <= now()",
        )
        .bind(i32::try_from(MAX_ATTEMPTS).unwrap_or(i32::MAX))
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        let rows: Vec<Claimed> = sqlx::query_as(
            "WITH due AS (
                 SELECT id FROM scheduled_jobs
                 WHERE state = 'pending' AND run_at <= now() AND attempts < $3
                   AND (leased_until IS NULL OR leased_until <= now())
                 ORDER BY run_at, id
                 LIMIT $1
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE scheduled_jobs AS job
             SET attempts = job.attempts + 1,
                 leased_until = now() + $2 * interval '1 second',
                 claim_token = nextval('scheduled_job_claims'),
                 updated_at = now()
             FROM due WHERE job.id = due.id
             RETURNING job.id, job.guild_id, job.module, job.name, job.payload, job.attempts,
                       job.claim_token",
        )
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .bind(seconds(lease))
        .bind(i32::try_from(MAX_ATTEMPTS).unwrap_or(i32::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        let mut jobs = rows
            .into_iter()
            .map(job_of)
            .collect::<Result<Vec<_>, _>>()?;
        jobs.sort_by_key(|job| job.id);
        Ok(jobs)
    }

    async fn complete(&self, id: i64, token: i64) -> Result<(), ScheduleError> {
        // A job that repeats is set for its next planned time that is still ahead; the runs missed
        // while the bot was down are not made up for. Only the hand-out that holds the job can
        // say it is done.
        sqlx::query(
            "WITH removed AS (
                 DELETE FROM scheduled_jobs
                 WHERE id = $1 AND claim_token = $2 AND every_seconds IS NULL
             )
             UPDATE scheduled_jobs
             SET planned_at = planned_at + (every_seconds::double precision
                     * (GREATEST(floor(extract(epoch FROM now() - planned_at) / every_seconds), 0) + 1))
                     * interval '1 second',
                 run_at = planned_at + (every_seconds::double precision
                     * (GREATEST(floor(extract(epoch FROM now() - planned_at) / every_seconds), 0) + 1))
                     * interval '1 second',
                 attempts = 0, leased_until = NULL, claim_token = 0, last_error = NULL,
                 updated_at = now()
             WHERE id = $1 AND claim_token = $2 AND every_seconds IS NOT NULL",
        )
        .bind(id)
        .bind(token)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn postpone(&self, id: i64, token: i64, after: Duration) -> Result<(), ScheduleError> {
        sqlx::query(
            "UPDATE scheduled_jobs
             SET attempts = GREATEST(attempts - 1, 0), leased_until = NULL, claim_token = 0,
                 run_at = now() + $3 * interval '1 second', updated_at = now()
             WHERE id = $1 AND claim_token = $2",
        )
        .bind(id)
        .bind(token)
        .bind(seconds(after))
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn retry(
        &self,
        id: i64,
        token: i64,
        after: Duration,
        error: &str,
    ) -> Result<(), ScheduleError> {
        sqlx::query(
            "UPDATE scheduled_jobs
             SET state = CASE WHEN attempts >= $4 THEN 'failed' ELSE 'pending' END,
                 leased_until = NULL, claim_token = 0, last_error = left($5, 1000),
                 updated_at = now(), run_at = now() + $3 * interval '1 second'
             WHERE id = $1 AND claim_token = $2",
        )
        .bind(id)
        .bind(token)
        .bind(seconds(after))
        .bind(i32::try_from(MAX_ATTEMPTS).unwrap_or(i32::MAX))
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn discard(&self, id: i64, token: i64) -> Result<(), ScheduleError> {
        sqlx::query("DELETE FROM scheduled_jobs WHERE id = $1 AND claim_token = $2")
            .bind(id)
            .bind(token)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }
}

impl Db {
    /// Deletes the jobs that were given up on more than two weeks ago. Run it now and then.
    pub async fn purge_scheduled_jobs(&self) -> Result<u64, DbError> {
        Ok(sqlx::query(
            "DELETE FROM scheduled_jobs
             WHERE state = 'failed' AND updated_at < now() - interval '14 days'",
        )
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::capability::{Capability, CapabilitySet};
    use closure_core::schedule::{Scheduler, backoff};
    use serde_json::json;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    async fn database(pool: PgPool) -> Db {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        db
    }

    fn scheduler(db: &Db, module: &str, guild: u64) -> Scheduler {
        let granted: CapabilitySet = [Capability::Schedule].into_iter().collect();
        Scheduler::new(
            Arc::new(db.clone()) as Arc<dyn SchedulerBackend>,
            &granted,
            ModuleId::new(module).unwrap(),
            Id::new(guild),
        )
    }

    const LEASE: Duration = Duration::from_secs(60);

    /// Moves every job's planned time back, as if that much time had passed.
    async fn age(db: &Db, seconds: i64) {
        sqlx::query(
            "UPDATE scheduled_jobs SET
                 run_at = run_at - $1 * interval '1 second',
                 planned_at = planned_at - $1 * interval '1 second',
                 leased_until = leased_until - $1 * interval '1 second'",
        )
        .bind(seconds)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    async fn count(db: &Db) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM scheduled_jobs")
            .fetch_one(&db.pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn a_job_is_handed_out_when_due_and_not_again_while_it_runs(pool: PgPool) {
        let db = database(pool).await;
        let warn = scheduler(&db, "warn", 1);
        warn.job("expire", &json!({ "user": 7 }))
            .unwrap()
            .after(Duration::from_secs(3600))
            .await
            .unwrap();
        warn.job("now", &1)
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();

        let jobs = db.claim_due(10, LEASE).await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!((jobs[0].name.as_str(), jobs[0].attempt), ("now", 1));
        assert_eq!(jobs[0].guild_id, Id::new(1));
        assert!(db.claim_due(10, LEASE).await.unwrap().is_empty());

        // The runner died: after the lease the job comes again, counted.
        age(&db, 120).await;
        let again = db.claim_due(10, LEASE).await.unwrap();
        assert_eq!(again[0].attempt, 2);
        db.complete(again[0].id, again[0].token).await.unwrap();
        assert_eq!(count(&db).await, 1, "only the job that is not due is left");

        age(&db, 3600).await;
        let payload = db.claim_due(10, LEASE).await.unwrap().remove(0);
        assert_eq!(payload.payload, json!({ "user": 7 }));
    }

    #[sqlx::test]
    async fn a_repeating_job_is_set_for_the_next_time_ahead(pool: PgPool) {
        let db = database(pool).await;
        scheduler(&db, "warn", 1)
            .job("tick", &())
            .unwrap()
            .every(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();
        let job = db.claim_due(1, LEASE).await.unwrap().remove(0);
        db.complete(job.id, job.token).await.unwrap();
        assert!(db.claim_due(1, LEASE).await.unwrap().is_empty());
        age(&db, 61).await;
        let job = db.claim_due(1, LEASE).await.unwrap().remove(0);
        assert_eq!(job.attempt, 1, "a new run starts counting again");

        // Down for a long time: one run, not one for every minute missed.
        age(&db, 10_000).await;
        db.complete(job.id, job.token).await.unwrap();
        assert!(db.claim_due(1, LEASE).await.unwrap().is_empty());
        let next: f64 = sqlx::query_scalar(
            "SELECT extract(epoch FROM run_at - now())::float8 FROM scheduled_jobs",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!((0.0..=60.0).contains(&next), "{next}");
    }

    #[sqlx::test]
    async fn keys_replace_and_cancel_and_belong_to_the_module_and_guild(pool: PgPool) {
        let db = database(pool).await;
        let warn = scheduler(&db, "warn", 1);
        warn.job("unmute", &1)
            .unwrap()
            .key("user:7")
            .after(Duration::from_secs(100))
            .await
            .unwrap();
        warn.job("unmute", &2)
            .unwrap()
            .key("user:7")
            .after(Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(count(&db).await, 1);
        assert_eq!(db.claim_due(5, LEASE).await.unwrap()[0].payload, json!(2));

        scheduler(&db, "mute", 1)
            .job("x", &0)
            .unwrap()
            .key("user:7")
            .after(Duration::ZERO)
            .await
            .unwrap();
        scheduler(&db, "warn", 2)
            .job("x", &0)
            .unwrap()
            .key("user:7")
            .after(Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(count(&db).await, 3);
        assert!(warn.cancel("user:7").await.unwrap());
        assert!(!warn.cancel("user:7").await.unwrap());
        assert_eq!(count(&db).await, 2);
    }

    #[sqlx::test]
    async fn a_failing_job_waits_longer_each_time_and_is_kept_when_given_up_on(pool: PgPool) {
        let db = database(pool).await;
        scheduler(&db, "warn", 1)
            .job("x", &1)
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        let mut last = 0;
        loop {
            let jobs = db.claim_due(1, LEASE).await.unwrap();
            let Some(job) = jobs.first() else { break };
            last = job.attempt;
            db.retry(job.id, job.token, backoff(job.attempt), "boom")
                .await
                .unwrap();
            assert!(db.claim_due(1, LEASE).await.unwrap().is_empty(), "it waits");
            age(&db, 3601).await;
        }
        assert_eq!(last, MAX_ATTEMPTS);
        let (state, error): (String, String) =
            sqlx::query_as("SELECT state, last_error FROM scheduled_jobs")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!((state.as_str(), error.as_str()), ("failed", "boom"));
        // Given-up jobs are cleaned away after a while, and not before.
        assert_eq!(db.purge_scheduled_jobs().await.unwrap(), 0);
        sqlx::query("UPDATE scheduled_jobs SET updated_at = now() - interval '15 days'")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(db.purge_scheduled_jobs().await.unwrap(), 1);
    }

    #[sqlx::test]
    async fn a_job_that_keeps_getting_lost_is_given_up_on(pool: PgPool) {
        let db = database(pool).await;
        scheduler(&db, "warn", 1)
            .job("x", &1)
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        for attempt in 1..=MAX_ATTEMPTS {
            assert_eq!(db.claim_due(1, LEASE).await.unwrap()[0].attempt, attempt);
            age(&db, 61).await;
        }
        assert!(db.claim_due(1, LEASE).await.unwrap().is_empty());
        let state: String = sqlx::query_scalar("SELECT state FROM scheduled_jobs")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(state, "failed");
    }

    #[sqlx::test]
    async fn postponing_does_not_count_and_discarding_removes(pool: PgPool) {
        let db = database(pool).await;
        let warn = scheduler(&db, "warn", 1);
        warn.job("x", &1)
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        let job = db.claim_due(1, LEASE).await.unwrap().remove(0);
        db.postpone(job.id, job.token, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(db.claim_due(1, LEASE).await.unwrap().is_empty());
        age(&db, 31).await;
        let job = db.claim_due(1, LEASE).await.unwrap().remove(0);
        assert_eq!(job.attempt, 1);
        db.discard(job.id, job.token).await.unwrap();
        assert_eq!(count(&db).await, 0);
    }

    #[sqlx::test]
    async fn runners_never_take_the_same_job(pool: PgPool) {
        let db = database(pool).await;
        let warn = scheduler(&db, "warn", 1);
        for n in 0..40 {
            warn.job("x", &n)
                .unwrap()
                .after(Duration::ZERO)
                .await
                .unwrap();
        }
        let runners = (0..4).map(|_| {
            let db = db.clone();
            tokio::spawn(async move { db.claim_due(40, LEASE).await.unwrap() })
        });
        let mut ids = Vec::new();
        for runner in runners {
            ids.extend(runner.await.unwrap().into_iter().map(|job| job.id));
        }
        let taken = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!((taken, ids.len()), (40, 40));
    }

    #[sqlx::test]
    async fn the_quota_is_exact_under_concurrency_and_a_time_is_kept_exactly(pool: PgPool) {
        let db = database(pool).await;
        let module = ModuleId::new("warn").unwrap();
        sqlx::query(
            "INSERT INTO scheduled_jobs (module, guild_id, name, payload, run_at, planned_at)
             SELECT 'warn', 1, 'x', '1', now() + interval '1 day', now() + interval '1 day'
             FROM generate_series(1, $1)",
        )
        .bind(i64::try_from(MAX_JOBS - 5).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
        let attempts = (0..20).map(|_| {
            let warn = scheduler(&db, "warn", 1);
            tokio::spawn(async move {
                warn.job("x", &1)
                    .unwrap()
                    .after(Duration::from_secs(5))
                    .await
            })
        });
        let mut added = 0;
        for attempt in attempts {
            match attempt.await.unwrap() {
                Ok(()) => added += 1,
                Err(error) => assert_eq!(error, ScheduleError::QuotaExceeded { limit: MAX_JOBS }),
            }
        }
        assert_eq!(added, 5);
        // Another guild has its own room; a time given in unix seconds is that time.
        let other = scheduler(&db, "warn", 2);
        other
            .job("at", &1)
            .unwrap()
            .key("k")
            .at(1_900_000_000)
            .await
            .unwrap();
        let at: i64 = sqlx::query_scalar(
            "SELECT extract(epoch FROM run_at)::bigint FROM scheduled_jobs WHERE guild_id = 2",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(at, 1_900_000_000);
        assert!(other.cancel("k").await.unwrap());
        assert!(!db.cancel(&module, Id::new(2), "k").await.unwrap());
    }
    #[sqlx::test]
    async fn a_report_about_a_job_that_was_replaced_or_handed_out_again_changes_nothing(
        pool: PgPool,
    ) {
        let db = database(pool).await;
        let warn = scheduler(&db, "warn", 1);
        warn.job("cleanup", &1)
            .unwrap()
            .key("c")
            .after(Duration::ZERO)
            .await
            .unwrap();
        let old = db.claim_due(1, LEASE).await.unwrap().remove(0);
        // The handler sets the next one under the same key; then the old run reports.
        warn.job("cleanup", &2)
            .unwrap()
            .key("c")
            .after(Duration::from_secs(600))
            .await
            .unwrap();
        db.complete(old.id, old.token).await.unwrap();
        db.retry(old.id, old.token, Duration::ZERO, "x")
            .await
            .unwrap();
        db.postpone(old.id, old.token, Duration::ZERO)
            .await
            .unwrap();
        db.discard(old.id, old.token).await.unwrap();
        assert_eq!(count(&db).await, 1);
        age(&db, 601).await;
        assert_eq!(db.claim_due(1, LEASE).await.unwrap()[0].payload, json!(2));

        // Handed out again after the lease: only the newest hand-out can report.
        age(&db, 120).await;
        let newer = db.claim_due(1, LEASE).await.unwrap().remove(0);
        db.complete(newer.id, newer.token - 1).await.unwrap();
        assert_eq!(count(&db).await, 1);
        db.complete(newer.id, newer.token).await.unwrap();
        assert_eq!(count(&db).await, 0);
    }

    #[sqlx::test]
    async fn a_retry_does_not_move_the_period_of_a_repeating_job(pool: PgPool) {
        let db = database(pool).await;
        scheduler(&db, "warn", 1)
            .job("tick", &())
            .unwrap()
            .every(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();
        let job = db.claim_due(1, LEASE).await.unwrap().remove(0);
        db.retry(job.id, job.token, Duration::from_secs(5), "boom")
            .await
            .unwrap();
        // Tried again 5 seconds after the planned time, and done.
        age(&db, 5).await;
        let again = db.claim_due(1, LEASE).await.unwrap().remove(0);
        db.complete(again.id, again.token).await.unwrap();
        // The next run is 60 seconds after the *planned* time, so 55 seconds from now.
        let ahead: f64 = sqlx::query_scalar(
            "SELECT extract(epoch FROM run_at - now())::float8 FROM scheduled_jobs",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!((50.0..=56.0).contains(&ahead), "{ahead}");
    }
}
