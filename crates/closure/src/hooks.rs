use std::sync::Arc;

use async_trait::async_trait;
use closure_config::SettingsStore;
use closure_core::event::GuildId;
use closure_core::locale::Locale;
use closure_db::Db;
use closure_discord::GuildHooks;

use crate::sync;

/// Keeps persistent state in step with the guilds the bot is in. Failures are logged and never
/// stop event processing: a database hiccup must not make the bot deaf.
pub struct AppHooks {
    db: Db,
    store: Arc<SettingsStore>,
}

impl AppHooks {
    pub fn new(db: Db, store: Arc<SettingsStore>) -> Self {
        Self { db, store }
    }
}

#[async_trait]
impl GuildHooks for AppHooks {
    async fn guild_available(&self, guild: GuildId, name: &str, preferred_locale: Option<Locale>) {
        tracing::debug!(%guild, name, "guild available");
        self.store.set_preferred_locale(guild, preferred_locale);
        if let Err(error) = self.db.register_guild(guild).await {
            tracing::warn!(%guild, %error, "could not record the guild");
        }
        // Configurations are loaded at start-up; a guild the bot rejoined after leaving was
        // dropped from memory and needs its stored configuration back.
        if !self.store.is_configured(guild)
            && let Err(error) = sync::reload_one(&self.db, &self.store, guild).await
        {
            tracing::warn!(%guild, %error, "could not load the guild's configuration");
        }
    }

    async fn guild_removed(&self, guild: GuildId) {
        tracing::info!(%guild, "removed from guild");
        self.store.remove_guild(guild);
        if let Err(error) = self.db.mark_guild_left(guild).await {
            tracing::warn!(%guild, %error, "could not record that the bot left the guild");
        }
    }
}

#[cfg(test)]
mod tests {
    use closure_config::{ConfigCompiler, parse_overrides};
    use closure_core::registry::ModuleRegistry;
    use closure_core::settings::SettingsProvider;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    fn store() -> Arc<SettingsStore> {
        let registry = Arc::new(ModuleRegistry::new(vec![]).unwrap());
        Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()))
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn a_new_guild_is_recorded_and_its_discord_language_remembered(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let store = store();
        let hooks = AppHooks::new(db, Arc::clone(&store));

        hooks
            .guild_available(Id::new(5), "Test", Some(Locale::Ja))
            .await;

        assert_eq!(store.locale(Id::new(5)), Locale::Ja);
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM guilds WHERE id = 5 AND left_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn leaving_and_rejoining_restores_the_stored_configuration(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let store = store();
        let hooks = AppHooks::new(db.clone(), Arc::clone(&store));
        let guild = Id::new(5);
        db.save_config(guild, None, &parse_overrides("language: ja").unwrap(), None)
            .await
            .unwrap();
        sync::load_all(&db, &store).await.unwrap();
        assert!(store.is_configured(guild));

        hooks.guild_removed(guild).await;
        assert!(!store.is_configured(guild));
        let left: bool = sqlx::query_scalar("SELECT left_at IS NOT NULL FROM guilds WHERE id = 5")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(left);
        assert_eq!(
            store.locale(guild),
            Locale::En,
            "defaults while the bot is away"
        );

        hooks.guild_available(guild, "Test", None).await;
        assert!(store.is_configured(guild));
        assert_eq!(
            store.locale(guild),
            Locale::Ja,
            "the configuration came back"
        );
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn database_failures_do_not_panic_or_block(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let hooks = AppHooks::new(db, store());
        pool.close().await;
        hooks
            .guild_available(Id::new(5), "Test", Some(Locale::Ja))
            .await;
        hooks.guild_removed(Id::new(5)).await;
    }
}
