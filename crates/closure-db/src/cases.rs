//! Moderation cases (see `closure_core::cases`).

use async_trait::async_trait;
use closure_core::cases::{Case, CaseBackend, CaseError, CaseKind, MAX_CASES, NewCase, Revoked};
use closure_core::event::{GuildId, UserId};
use closure_core::module::ModuleId;

use crate::Db;
use crate::ids::{from_db, to_db};

/// The columns of a case, with the times as unix seconds. A macro so the queries stay literals.
macro_rules! case_columns {
    () => {
        "guild_id, number, module, kind, target, moderator, reason,
         extract(epoch FROM created_at)::bigint, extract(epoch FROM expires_at)::bigint,
         revoked_by, extract(epoch FROM revoked_at)::bigint, revoked_reason"
    };
}

type CaseRow = (
    i64,
    i64,
    String,
    String,
    i64,
    i64,
    Option<String>,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// Takes the next number of a guild. The row stays locked until the transaction ends, so two
/// creations never get the same number.
const NEXT_NUMBER: &str = "INSERT INTO guild_case_numbers (guild_id, last) VALUES ($1, 1)
     ON CONFLICT (guild_id) DO UPDATE SET last = guild_case_numbers.last + 1
     RETURNING last";

fn backend(error: impl std::fmt::Display) -> CaseError {
    CaseError::Backend(error.to_string())
}

fn case_from_row(
    (
        guild,
        number,
        module,
        kind,
        target,
        moderator,
        reason,
        created_at,
        expires_at,
        revoked_by,
        revoked_at,
        revoked_reason,
    ): CaseRow,
) -> Result<Case, CaseError> {
    let revoked = match (revoked_by, revoked_at) {
        (Some(by), Some(at)) => Some(Revoked {
            by: from_db(by).map_err(backend)?,
            at,
            reason: revoked_reason,
        }),
        (None, None) => None,
        _ => return Err(backend("a revocation without its time or its revoker")),
    };
    Ok(Case {
        number,
        guild_id: from_db(guild).map_err(backend)?,
        module: ModuleId::new(module).map_err(backend)?,
        kind: CaseKind::new(&kind).map_err(backend)?,
        target: from_db(target).map_err(backend)?,
        moderator: from_db(moderator).map_err(backend)?,
        reason,
        created_at,
        expires_at,
        revoked,
    })
}

#[async_trait]
impl CaseBackend for Db {
    async fn create(
        &self,
        module: &ModuleId,
        guild: GuildId,
        case: NewCase,
    ) -> Result<Case, CaseError> {
        let guild = to_db(guild).map_err(backend)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        let number: i64 = sqlx::query_scalar(NEXT_NUMBER)
            .bind(guild)
            .fetch_one(&mut *tx)
            .await
            .map_err(backend)?;
        if number > MAX_CASES {
            tx.rollback().await.map_err(backend)?;
            return Err(CaseError::QuotaExceeded { limit: MAX_CASES });
        }
        let row: CaseRow = sqlx::query_as(concat!(
            "INSERT INTO cases (guild_id, number, module, kind, target, moderator, reason, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, to_timestamp($8::bigint))
             RETURNING ",
            case_columns!()
        ))
        .bind(guild)
        .bind(number)
        .bind(module.as_str())
        .bind(case.kind.as_str())
        .bind(to_db(case.target).map_err(backend)?)
        .bind(to_db(case.moderator).map_err(backend)?)
        .bind(case.reason.as_deref())
        .bind(case.expires_at)
        .fetch_one(&mut *tx)
        .await
        .map_err(backend)?;
        tx.commit().await.map_err(backend)?;
        case_from_row(row)
    }

    async fn get(&self, guild: GuildId, number: i64) -> Result<Option<Case>, CaseError> {
        let row: Option<CaseRow> = sqlx::query_as(concat!(
            "SELECT ",
            case_columns!(),
            " FROM cases WHERE guild_id = $1 AND number = $2"
        ))
        .bind(to_db(guild).map_err(backend)?)
        .bind(number)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(case_from_row).transpose()
    }

    async fn list_for_target(
        &self,
        guild: GuildId,
        target: UserId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        let rows: Vec<CaseRow> = sqlx::query_as(concat!(
            "SELECT ",
            case_columns!(),
            " FROM cases
             WHERE guild_id = $1 AND target = $2 AND ($3::bigint IS NULL OR number < $3)
             ORDER BY number DESC LIMIT $4"
        ))
        .bind(to_db(guild).map_err(backend)?)
        .bind(to_db(target).map_err(backend)?)
        .bind(before)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(case_from_row).collect()
    }

    async fn list_recent(
        &self,
        guild: GuildId,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        let rows: Vec<CaseRow> = sqlx::query_as(concat!(
            "SELECT ",
            case_columns!(),
            " FROM cases
             WHERE guild_id = $1 AND ($2::bigint IS NULL OR number < $2)
             ORDER BY number DESC LIMIT $3"
        ))
        .bind(to_db(guild).map_err(backend)?)
        .bind(before)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(case_from_row).collect()
    }

    async fn list_standing(
        &self,
        guild: GuildId,
        target: UserId,
        kind: &str,
        now: i64,
        limit: usize,
    ) -> Result<Vec<Case>, CaseError> {
        let rows: Vec<CaseRow> = sqlx::query_as(concat!(
            "SELECT ",
            case_columns!(),
            " FROM cases
             WHERE guild_id = $1 AND target = $2 AND kind = $3 AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > to_timestamp($4::bigint))
             ORDER BY number DESC LIMIT $5"
        ))
        .bind(to_db(guild).map_err(backend)?)
        .bind(to_db(target).map_err(backend)?)
        .bind(kind)
        .bind(now)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(case_from_row).collect()
    }

    async fn revoke(
        &self,
        guild: GuildId,
        number: i64,
        by: UserId,
        reason: Option<String>,
    ) -> Result<Case, CaseError> {
        let guild = to_db(guild).map_err(backend)?;
        let row: Option<CaseRow> = sqlx::query_as(concat!(
            "UPDATE cases SET revoked_by = $3, revoked_at = now(), revoked_reason = $4
             WHERE guild_id = $1 AND number = $2 AND revoked_at IS NULL
             RETURNING ",
            case_columns!()
        ))
        .bind(guild)
        .bind(number)
        .bind(to_db(by).map_err(backend)?)
        .bind(reason)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        if let Some(row) = row {
            return case_from_row(row);
        }
        // Nothing was revoked: the case is missing, or it was revoked before.
        let revoked: Option<bool> = sqlx::query_scalar(
            "SELECT revoked_at IS NOT NULL FROM cases WHERE guild_id = $1 AND number = $2",
        )
        .bind(guild)
        .bind(number)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Err(match revoked {
            Some(true) => CaseError::AlreadyRevoked,
            Some(false) | None => CaseError::NotFound,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::capability::{Capability, CapabilitySet};
    use closure_core::cases::{
        Case, CaseBackend, CaseError, CaseKind, Cases, MAX_CASES, MemoryCases, NewCase,
    };
    use closure_core::module::ModuleId;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    fn cases(db: &Db, module: &str, guild: u64) -> Cases {
        let granted: CapabilitySet = [Capability::Cases].into_iter().collect();
        Cases::new(
            Arc::new(db.clone()) as Arc<dyn CaseBackend>,
            &granted,
            ModuleId::new(module).unwrap(),
            Id::new(guild),
        )
    }

    fn new_case(target: u64, reason: Option<&str>) -> NewCase {
        NewCase {
            kind: CaseKind::new("warn").unwrap(),
            target: Id::new(target),
            moderator: Id::new(9),
            reason: reason.map(str::to_owned),
            expires_at: None,
        }
    }

    fn numbers(cases: &[Case]) -> Vec<i64> {
        cases.iter().map(|case| case.number).collect()
    }

    /// The case without the times, which the two backends take from their own clocks.
    fn tidy(case: &Case) -> Case {
        let mut case = case.clone();
        case.created_at = 0;
        if let Some(revoked) = case.revoked.as_mut() {
            revoked.at = 0;
        }
        case
    }

    #[sqlx::test]
    async fn a_created_case_reads_back_whole(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let warn = cases(&db, "warn", 1);
        let created = warn
            .create(NewCase {
                kind: CaseKind::new("timeout").unwrap(),
                target: Id::new(5),
                moderator: Id::new(9),
                reason: Some("spam".into()),
                expires_at: Some(1_900_000_000),
            })
            .await
            .unwrap();
        assert_eq!(created.number, 1);
        assert_eq!(created.module, ModuleId::new("warn").unwrap());
        assert_eq!(created.guild_id, Id::new(1));
        assert!(created.created_at > 1_700_000_000, "the database's clock");
        assert_eq!(created.expires_at, Some(1_900_000_000));
        assert_eq!(created.revoked, None);
        assert_eq!(warn.get(1).await.unwrap(), Some(created));
        assert_eq!(warn.get(2).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn numbers_run_per_guild_and_modules_share_them(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let (warn, mute, elsewhere) = (
            cases(&db, "warn", 1),
            cases(&db, "mute", 1),
            cases(&db, "warn", 2),
        );
        assert_eq!(warn.create(new_case(5, None)).await.unwrap().number, 1);
        assert_eq!(mute.create(new_case(5, None)).await.unwrap().number, 2);
        assert_eq!(elsewhere.create(new_case(5, None)).await.unwrap().number, 1);
        assert_eq!(
            warn.get(2).await.unwrap().unwrap().module,
            ModuleId::new("mute").unwrap()
        );
    }

    #[sqlx::test]
    async fn concurrent_creates_take_1_to_20_without_gaps_or_repeats(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let warn = cases(&db, "warn", 1);
        let tasks: Vec<_> = (0..20)
            .map(|n| {
                let warn = warn.clone();
                tokio::spawn(async move { warn.create(new_case(n + 1, None)).await })
            })
            .collect();
        let mut taken = Vec::new();
        for task in tasks {
            taken.push(task.await.unwrap().unwrap().number);
        }
        taken.sort_unstable();
        assert_eq!(taken, (1..=20).collect::<Vec<_>>());
    }

    #[sqlx::test]
    async fn of_two_concurrent_revokes_only_one_succeeds(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let warn = cases(&db, "warn", 1);
        warn.create(new_case(5, None)).await.unwrap();
        let (first, second) = (warn.clone(), warn.clone());
        let (a, b) = tokio::join!(
            tokio::spawn(async move { first.revoke(1, Id::new(8), None).await }),
            tokio::spawn(async move { second.revoke(1, Id::new(7), None).await }),
        );
        let results = [a.unwrap(), b.unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert!(results.contains(&Err(CaseError::AlreadyRevoked)));
    }

    #[sqlx::test]
    async fn listings_page_newest_first_per_target_and_for_the_guild(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let warn = cases(&db, "warn", 1);
        for target in [5, 7, 5, 5, 7, 5] {
            warn.create(new_case(target, None)).await.unwrap();
        }
        let first = warn.for_target(Id::new(5), None, 2).await.unwrap();
        assert_eq!(numbers(&first), [6, 4]);
        let second = warn.for_target(Id::new(5), Some(4), 2).await.unwrap();
        assert_eq!(numbers(&second), [3, 1]);
        assert!(
            warn.for_target(Id::new(5), Some(1), 2)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            numbers(&warn.for_target(Id::new(7), None, 10).await.unwrap()),
            [5, 2]
        );
        assert_eq!(numbers(&warn.recent(None, 3).await.unwrap()), [6, 5, 4]);
        assert_eq!(numbers(&warn.recent(Some(3), 10).await.unwrap()), [2, 1]);
    }

    #[sqlx::test]
    async fn the_quota_refuses_the_next_number_and_takes_none(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        sqlx::query("INSERT INTO guild_case_numbers (guild_id, last) VALUES (1, $1)")
            .bind(MAX_CASES)
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            cases(&db, "warn", 1).create(new_case(5, None)).await,
            Err(CaseError::QuotaExceeded { limit: MAX_CASES })
        );
        let last: i64 =
            sqlx::query_scalar("SELECT last FROM guild_case_numbers WHERE guild_id = 1")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(last, MAX_CASES);
        assert_eq!(
            cases(&db, "warn", 2)
                .create(new_case(5, None))
                .await
                .unwrap()
                .number,
            1
        );
    }

    #[sqlx::test]
    async fn a_revocation_is_kept_and_happens_once(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let warn = cases(&db, "warn", 1);
        warn.create(new_case(5, None)).await.unwrap();
        let revoked = warn
            .revoke(1, Id::new(9), Some("mistake".into()))
            .await
            .unwrap();
        let by_then = revoked.revoked.clone().unwrap();
        assert_eq!(by_then.by, Id::new(9));
        assert_eq!(by_then.reason.as_deref(), Some("mistake"));
        assert!(by_then.at > 1_700_000_000);
        assert_eq!(warn.get(1).await.unwrap().unwrap().revoked, Some(by_then));
        assert_eq!(
            warn.revoke(1, Id::new(10), None).await,
            Err(CaseError::AlreadyRevoked)
        );
        assert_eq!(
            warn.revoke(2, Id::new(10), None).await,
            Err(CaseError::NotFound)
        );
    }

    #[sqlx::test]
    async fn another_guilds_cases_are_not_visible(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        cases(&db, "warn", 1)
            .create(new_case(5, None))
            .await
            .unwrap();
        let elsewhere = cases(&db, "warn", 2);
        assert_eq!(elsewhere.get(1).await.unwrap(), None);
        assert!(elsewhere.recent(None, 10).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn the_standing_search_skips_other_kinds_revoked_and_ended_cases(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let warn = cases(&db, "warn", 1);
        let ban = |expires_at: Option<i64>| NewCase {
            kind: CaseKind::new("ban").unwrap(),
            target: Id::new(5),
            moderator: Id::new(9),
            reason: None,
            expires_at,
        };
        warn.create(ban(Some(1_900_000_000))).await.unwrap();
        warn.create(ban(Some(1_600_000_000))).await.unwrap();
        warn.create(ban(None)).await.unwrap();
        warn.create(new_case(5, None)).await.unwrap();
        warn.create(ban(None)).await.unwrap();
        warn.revoke(3, Id::new(8), None).await.unwrap();

        let standing = warn
            .standing(Id::new(5), "ban", 1_700_000_000, 10)
            .await
            .unwrap();
        assert_eq!(numbers(&standing), [5, 1]);
        assert_eq!(
            numbers(
                &warn
                    .standing(Id::new(5), "ban", 1_700_000_000, 1)
                    .await
                    .unwrap()
            ),
            [5]
        );
    }

    /// Runs the same calls and records what each answered, to compare the two backends.
    async fn script(cases: &Cases) -> Vec<String> {
        let mut log = Vec::new();
        log.push(format!(
            "{:?}",
            cases
                .create(new_case(5, Some("spam")))
                .await
                .map(|case| tidy(&case))
        ));
        log.push(format!(
            "{:?}",
            cases
                .create(new_case(7, None))
                .await
                .map(|case| tidy(&case))
        ));
        log.push(format!(
            "{:?}",
            cases
                .create(new_case(5, Some(&"x".repeat(513))))
                .await
                .map(|case| tidy(&case))
        ));
        log.push(format!(
            "{:?}",
            cases.get(1).await.map(|case| case.map(|case| tidy(&case)))
        ));
        log.push(format!(
            "{:?}",
            cases
                .for_target(Id::new(5), None, 10)
                .await
                .map(|found| found.iter().map(tidy).collect::<Vec<_>>())
        ));
        log.push(format!(
            "{:?}",
            cases
                .recent(Some(2), 10)
                .await
                .map(|found| found.iter().map(tidy).collect::<Vec<_>>())
        ));
        log.push(format!(
            "{:?}",
            cases
                .revoke(1, Id::new(9), Some("mistake".into()))
                .await
                .map(|case| tidy(&case))
        ));
        log.push(format!(
            "{:?}",
            cases
                .revoke(1, Id::new(9), None)
                .await
                .map(|case| tidy(&case))
        ));
        log.push(format!(
            "{:?}",
            cases
                .revoke(42, Id::new(9), None)
                .await
                .map(|case| tidy(&case))
        ));
        log
    }

    #[sqlx::test]
    async fn the_database_and_the_memory_backend_agree(pool: PgPool) {
        // Same calls, same answers: the memory backend is what tests of modules rely on.
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let granted: CapabilitySet = [Capability::Cases].into_iter().collect();
        let in_memory = Cases::new(
            Arc::new(MemoryCases::with_clock(|| 1_700_000_000)) as Arc<dyn CaseBackend>,
            &granted,
            ModuleId::new("warn").unwrap(),
            Id::new(1),
        );
        let in_db = cases(&db, "warn", 1);
        assert_eq!(script(&in_memory).await, script(&in_db).await);
    }
}
