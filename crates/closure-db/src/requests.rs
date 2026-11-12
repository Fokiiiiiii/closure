//! Requests the API carries out once: a request that comes again with the same key is not carried
//! out again, whether the first one finished, is still under way, or was cut short by a stop.

use std::time::Duration;

use closure_core::event::GuildId;
use serde_json::Value;

use crate::Db;
use crate::error::DbError;
use crate::ids::to_db;

/// How long a request is remembered.
pub const REQUESTS_KEPT: Duration = Duration::from_hours(24);

/// What [`Db::claim_request`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Claim {
    /// Nobody asked with this key before: the caller carries the request out, then calls
    /// [`Db::finish_request`] (or [`Db::release_request`] when it failed and may be tried again).
    Fresh,
    /// The same request was carried out; this is what it answered.
    Done(Value),
    /// The same request was started and has not been reported done: it may be under way, or a stop
    /// may have cut it short after its effect. It is not carried out again.
    Pending,
    /// The key was used for another request.
    Different,
}

impl Db {
    /// Claims `key` of `actor` in `guild` for `request`.
    pub async fn claim_request(
        &self,
        guild: GuildId,
        actor: &str,
        key: &str,
        request: &Value,
    ) -> Result<Claim, DbError> {
        let guild = to_db(guild)?;
        let inserted = sqlx::query(
            "INSERT INTO api_requests (guild_id, actor, key, request) VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(guild)
        .bind(actor)
        .bind(key)
        .bind(request)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if inserted == 1 {
            return Ok(Claim::Fresh);
        }
        let found: Option<(Value, Option<Value>)> = sqlx::query_as(
            "SELECT request, response FROM api_requests
             WHERE guild_id = $1 AND actor = $2 AND key = $3",
        )
        .bind(guild)
        .bind(actor)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match found {
            Some((stored, _)) if stored != *request => Claim::Different,
            Some((_, Some(response))) => Claim::Done(response),
            // Released between the two statements: the caller asks again.
            Some((_, None)) | None => Claim::Pending,
        })
    }

    /// Records what a claimed request answered.
    pub async fn finish_request(
        &self,
        guild: GuildId,
        actor: &str,
        key: &str,
        response: &Value,
    ) -> Result<(), DbError> {
        sqlx::query(
            "UPDATE api_requests SET response = $4
             WHERE guild_id = $1 AND actor = $2 AND key = $3",
        )
        .bind(to_db(guild)?)
        .bind(actor)
        .bind(key)
        .bind(response)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Gives a claim up, for a request that failed before it had an effect, so that it can be
    /// asked again with the same key.
    pub async fn release_request(
        &self,
        guild: GuildId,
        actor: &str,
        key: &str,
    ) -> Result<(), DbError> {
        sqlx::query(
            "DELETE FROM api_requests
             WHERE guild_id = $1 AND actor = $2 AND key = $3 AND response IS NULL",
        )
        .bind(to_db(guild)?)
        .bind(actor)
        .bind(key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Forgets expired API requests and integration-secret idempotency receipts.
    pub async fn purge_requests(&self) -> Result<u64, DbError> {
        let result = sqlx::query(
            "DELETE FROM api_requests WHERE created_at < now() - make_interval(secs => $1)",
        )
        .bind(REQUESTS_KEPT.as_secs_f64())
        .execute(&self.pool)
        .await?;
        sqlx::query("DELETE FROM integration_secret_receipts WHERE expires_at <= now()")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    #[sqlx::test]
    async fn a_key_is_carried_out_once_and_remembers_its_answer(pool: PgPool) {
        let db = Db::from_pool(pool);
        let guild = Id::new(7);
        let request = json!({ "text": "hello" });
        assert_eq!(
            db.claim_request(guild, "1", "k", &request).await.unwrap(),
            Claim::Fresh
        );
        assert_eq!(
            db.claim_request(guild, "1", "k", &request).await.unwrap(),
            Claim::Pending
        );
        db.finish_request(guild, "1", "k", &json!({ "id": "9" }))
            .await
            .unwrap();
        assert_eq!(
            db.claim_request(guild, "1", "k", &request).await.unwrap(),
            Claim::Done(json!({ "id": "9" }))
        );
        assert_eq!(
            db.claim_request(guild, "1", "k", &json!({ "text": "other" }))
                .await
                .unwrap(),
            Claim::Different
        );
        assert_eq!(
            db.claim_request(guild, "2", "k", &request).await.unwrap(),
            Claim::Fresh
        );
        assert_eq!(
            db.claim_request(Id::new(8), "1", "k", &request)
                .await
                .unwrap(),
            Claim::Fresh
        );
    }

    #[sqlx::test]
    async fn a_failed_request_is_released_but_a_finished_one_is_not(pool: PgPool) {
        let db = Db::from_pool(pool);
        let guild = Id::new(7);
        let request = json!(1);
        db.claim_request(guild, "1", "a", &request).await.unwrap();
        db.release_request(guild, "1", "a").await.unwrap();
        assert_eq!(
            db.claim_request(guild, "1", "a", &request).await.unwrap(),
            Claim::Fresh
        );
        db.finish_request(guild, "1", "a", &json!("done"))
            .await
            .unwrap();
        db.release_request(guild, "1", "a").await.unwrap();
        assert_eq!(
            db.claim_request(guild, "1", "a", &request).await.unwrap(),
            Claim::Done(json!("done"))
        );
    }

    #[sqlx::test]
    async fn old_requests_are_forgotten(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.claim_request(Id::new(7), "1", "old", &json!(1))
            .await
            .unwrap();
        sqlx::query("UPDATE api_requests SET created_at = now() - interval '2 days'")
            .execute(&db.pool)
            .await
            .unwrap();
        db.claim_request(Id::new(7), "1", "new", &json!(1))
            .await
            .unwrap();
        assert_eq!(db.purge_requests().await.unwrap(), 1);
    }
}
