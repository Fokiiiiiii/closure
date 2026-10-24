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

    /// Whether the bot is currently in `guild`, as far as the database knows.
    pub async fn is_guild_active(&self, guild: GuildId) -> Result<bool, DbError> {
        let active = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM guilds WHERE id = $1 AND left_at IS NULL)",
        )
        .bind(to_db(guild)?)
        .fetch_one(&self.pool)
        .await?;
        Ok(active)
    }

    /// Marks every active guild not in `present` as left, for guilds the bot was removed from
    /// while it was not running. Returns how many were marked.
    pub async fn mark_missing_guilds_left(&self, present: &[GuildId]) -> Result<u64, DbError> {
        let present = present
            .iter()
            .map(|guild| to_db(*guild))
            .collect::<Result<Vec<_>, _>>()?;
        let result = sqlx::query(
            "UPDATE guilds SET left_at = now() WHERE left_at IS NULL AND id <> ALL($1)",
        )
        .bind(present)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
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
    async fn only_guilds_the_bot_is_in_are_active(pool: PgPool) {
        let db = Db::from_pool(pool);
        let guild = Id::new(42);
        assert!(!db.is_guild_active(guild).await.unwrap(), "never seen");

        db.register_guild(guild).await.unwrap();
        assert!(db.is_guild_active(guild).await.unwrap());

        db.mark_guild_left(guild).await.unwrap();
        assert!(!db.is_guild_active(guild).await.unwrap(), "left");
    }

    #[sqlx::test]
    async fn guilds_missing_from_discord_s_list_are_marked_left(pool: PgPool) {
        let db = Db::from_pool(pool);
        for id in [1, 2, 3] {
            db.register_guild(Id::new(id)).await.unwrap();
        }
        db.mark_guild_left(Id::new(3)).await.unwrap();

        let marked = db
            .mark_missing_guilds_left(&[Id::new(1), Id::new(9)])
            .await
            .unwrap();

        assert_eq!(marked, 1, "only guild 2 was active and absent");
        assert!(db.is_guild_active(Id::new(1)).await.unwrap());
        assert!(!db.is_guild_active(Id::new(2)).await.unwrap());
        assert_eq!(db.mark_missing_guilds_left(&[]).await.unwrap(), 1);
        assert!(!db.is_guild_active(Id::new(1)).await.unwrap());
    }

    #[sqlx::test]
    async fn leaving_an_unknown_guild_is_harmless(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.mark_guild_left(Id::new(7)).await.unwrap();
    }
}
