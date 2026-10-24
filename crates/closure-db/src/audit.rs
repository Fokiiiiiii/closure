use closure_core::event::{GuildId, UserId};
use time::OffsetDateTime;

use crate::Db;
use crate::error::DbError;
use crate::ids::{from_db, to_db};

const MAX_ACTION_CHARS: usize = 64;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const MAX_RECENT: u32 = 500;

/// Something the bot did that an administrator may later want to explain.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
    pub guild_id: GuildId,
    /// The user who caused it, or `None` for automatic actions.
    pub actor: Option<UserId>,
    /// A dotted lowercase name such as `config.update` or `member.ban`.
    pub action: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuditRecord {
    pub id: i64,
    pub guild_id: GuildId,
    pub actor: Option<UserId>,
    pub action: String,
    pub payload: serde_json::Value,
    pub created_at: OffsetDateTime,
}

fn valid_action(action: &str) -> bool {
    (1..=MAX_ACTION_CHARS).contains(&action.len())
        && action.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

impl Db {
    /// Appends an event and returns its id.
    pub async fn record_audit(&self, event: &AuditEvent) -> Result<i64, DbError> {
        if !valid_action(&event.action) {
            return Err(DbError::Invalid(format!(
                "audit action must be 1-{MAX_ACTION_CHARS} chars of [a-z0-9._-], got `{}`",
                event.action
            )));
        }
        let size = serde_json::to_vec(&event.payload)?.len();
        if size > MAX_PAYLOAD_BYTES {
            return Err(DbError::Invalid(format!(
                "audit payload is {size} bytes, over the {MAX_PAYLOAD_BYTES} byte limit"
            )));
        }
        let id = sqlx::query_scalar::<_, i64>(
            "INSERT INTO audit_events (guild_id, actor, action, payload)
             VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(to_db(event.guild_id)?)
        .bind(event.actor.map(to_db).transpose()?)
        .bind(&event.action)
        .bind(&event.payload)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// The most recent events of a guild, newest first.
    pub async fn recent_audit(
        &self,
        guild: GuildId,
        limit: u32,
    ) -> Result<Vec<AuditRecord>, DbError> {
        let rows = sqlx::query_as::<
            _,
            (
                i64,
                i64,
                Option<i64>,
                String,
                serde_json::Value,
                OffsetDateTime,
            ),
        >(
            "SELECT id, guild_id, actor, action, payload, created_at
             FROM audit_events WHERE guild_id = $1
             ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(to_db(guild)?)
        .bind(i64::from(limit.clamp(1, MAX_RECENT)))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|(id, guild_id, actor, action, payload, created_at)| {
                Ok(AuditRecord {
                    id,
                    guild_id: from_db(guild_id)?,
                    actor: actor.map(from_db).transpose()?,
                    action,
                    payload,
                    created_at,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    fn event(guild: u64, action: &str) -> AuditEvent {
        AuditEvent {
            guild_id: Id::new(guild),
            actor: Some(Id::new(9)),
            action: action.into(),
            payload: json!({ "k": "v" }),
        }
    }

    #[sqlx::test]
    async fn events_come_back_newest_first_and_per_guild(pool: PgPool) {
        let db = Db::from_pool(pool);
        let first = db.record_audit(&event(1, "config.update")).await.unwrap();
        let second = db.record_audit(&event(1, "member.ban")).await.unwrap();
        db.record_audit(&event(2, "member.kick")).await.unwrap();

        let records = db.recent_audit(Id::new(1), 10).await.unwrap();
        let ids: Vec<i64> = records.iter().map(|r| r.id).collect();
        assert_eq!(ids, [second, first]);
        assert_eq!(records[0].action, "member.ban");
        assert_eq!(records[0].actor, Some(Id::new(9)));
        assert_eq!(records[0].payload, json!({ "k": "v" }));
    }

    #[sqlx::test]
    async fn limit_is_applied_and_clamped(pool: PgPool) {
        let db = Db::from_pool(pool);
        for _ in 0..3 {
            db.record_audit(&event(1, "a.b")).await.unwrap();
        }
        assert_eq!(db.recent_audit(Id::new(1), 2).await.unwrap().len(), 2);
        assert_eq!(db.recent_audit(Id::new(1), 0).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    async fn invalid_actions_and_oversized_payloads_are_rejected(pool: PgPool) {
        let db = Db::from_pool(pool);
        for bad in ["", "Has Space", "UPPER", &"a".repeat(65)] {
            assert!(matches!(
                db.record_audit(&event(1, bad)).await,
                Err(DbError::Invalid(_))
            ));
        }
        let mut huge = event(1, "a.b");
        huge.payload = json!({ "blob": "x".repeat(MAX_PAYLOAD_BYTES) });
        assert!(matches!(
            db.record_audit(&huge).await,
            Err(DbError::Invalid(_))
        ));
        assert_eq!(db.recent_audit(Id::new(1), 10).await.unwrap(), []);
    }

    #[sqlx::test]
    async fn the_trail_does_not_need_a_registered_guild(pool: PgPool) {
        let db = Db::from_pool(pool);
        assert!(db.record_audit(&event(123, "system.start")).await.is_ok());
    }
}
