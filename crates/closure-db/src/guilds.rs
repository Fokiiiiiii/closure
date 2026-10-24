use closure_core::event::GuildId;

use crate::Db;
use crate::error::DbError;
use crate::ids::to_db;

impl Db {
    /// Records that the bot is in `guild`. Safe to call repeatedly, including when the bot
    /// rejoins a guild it left earlier.
    pub async fn register_guild(&self, guild: GuildId) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO guilds (id) VALUES ($1)
             ON CONFLICT (id) DO UPDATE SET left_at = NULL",
        )
        .bind(to_db(guild)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records that the bot left `guild`. Its data is kept in case it returns.
    pub async fn mark_guild_left(&self, guild: GuildId) -> Result<(), DbError> {
        sqlx::query("UPDATE guilds SET left_at = now() WHERE id = $1 AND left_at IS NULL")
            .bind(to_db(guild)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    async fn left_at_is_null(pool: &PgPool, guild: i64) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT left_at IS NULL FROM guilds WHERE id = $1")
            .bind(guild)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn registering_is_idempotent_and_leaving_is_reversible(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let guild = Id::new(42);

        db.register_guild(guild).await.unwrap();
        db.register_guild(guild).await.unwrap();
        assert!(left_at_is_null(&pool, 42).await);

        db.mark_guild_left(guild).await.unwrap();
        assert!(!left_at_is_null(&pool, 42).await);

        db.register_guild(guild).await.unwrap();
        assert!(left_at_is_null(&pool, 42).await);
    }

    #[sqlx::test]
    async fn leaving_an_unknown_guild_is_harmless(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.mark_guild_left(Id::new(7)).await.unwrap();
    }
}
