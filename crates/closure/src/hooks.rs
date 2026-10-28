use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use closure::message_history::{MessageRecorder, unix_now};
use closure_config::SettingsStore;
use closure_core::event::{Event as ModuleEvent, GuildId};
use closure_core::locale::Locale;
use closure_db::Db;
use closure_discord::GuildHooks;
use tokio::sync::mpsc::UnboundedSender;

use crate::sync;

/// Keeps persistent state in step with the guilds the bot is in. Failures are logged and never
/// stop event processing: a database hiccup must not make the bot deaf.
pub struct AppHooks {
    db: Db,
    store: Arc<SettingsStore>,
    messages: MessageRecorder,
    /// Where the guilds whose configuration to announce are sent (see [`crate::app`]).
    changes: UnboundedSender<GuildId>,
    announced: Announced,
}

/// The guilds whose configuration the modules have been told about since the bot joined them.
#[derive(Debug, Default)]
struct Announced(Mutex<HashSet<GuildId>>);

impl Announced {
    /// Whether `guild` was not announced yet; it is recorded as announced either way.
    fn first(&self, guild: GuildId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(guild)
    }

    /// Forgets `guild`, so that its next arrival is announced again.
    fn forget(&self, guild: GuildId) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&guild);
    }
}

impl AppHooks {
    /// `message_history_days` is how long kept messages stay (see [`MessageRecorder`]).
    pub fn new(
        db: Db,
        store: Arc<SettingsStore>,
        message_history_days: u32,
        changes: UnboundedSender<GuildId>,
    ) -> Self {
        let messages = MessageRecorder::new(
            Arc::new(db.clone()),
            Arc::clone(&store),
            message_history_days,
            Arc::new(closure_discord::HttpFetcher::new()),
        );
        Self {
            db,
            store,
            messages,
            changes,
            announced: Announced::default(),
        }
    }
}

/// Brings the recorded guilds in line with the list Discord reports at start-up: the gateway
/// only announces changes while connected, so guilds joined or left during downtime would
/// otherwise stay wrong. Run before the gateway starts so it cannot race with the hooks.
pub async fn reconcile_guilds(db: &Db, present: &[GuildId]) -> Result<u64, closure_db::DbError> {
    for guild in present {
        db.register_guild(*guild).await?;
    }
    db.mark_missing_guilds_left(present).await
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
        // Once per arrival, after the guild's event: the modules set up what their settings need.
        if self.store.is_configured(guild) && self.announced.first(guild) {
            let _ = self.changes.send(guild);
        }
    }

    async fn guild_removed(&self, guild: GuildId) {
        tracing::info!(%guild, "removed from guild");
        self.store.remove_guild(guild);
        self.announced.forget(guild);
        if let Err(error) = self.db.mark_guild_left(guild).await {
            tracing::warn!(%guild, %error, "could not record that the bot left the guild");
        }
    }

    fn wants_messages(&self, guild: GuildId) -> bool {
        self.messages.keeps(guild)
    }

    async fn message(&self, event: &mut ModuleEvent) {
        if let Err(error) = self.messages.record(event, unix_now()).await {
            tracing::warn!(kind = event.kind().as_str(), %error, "could not keep a message");
        }
    }
}

#[cfg(test)]
mod tests {
    use closure_config::{ConfigCompiler, parse_overrides};
    use closure_core::capability::Capability;
    use closure_core::event::{MessageInfo, UserInfo};
    use closure_core::module::{Module, ModuleId, ModuleManifest};
    use closure_core::registry::ModuleRegistry;
    use closure_core::settings::SettingsProvider;
    use closure_core::testing::StubModule;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    fn store() -> Arc<SettingsStore> {
        let registry = Arc::new(ModuleRegistry::new(vec![]).unwrap());
        Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()))
    }

    /// A store with one module that reads the message history; it is off until a guild enables it.
    fn store_with_a_reader() -> Arc<SettingsStore> {
        let mut manifest = ModuleManifest::new(ModuleId::new("reader").unwrap(), "reader", "0.1.0");
        manifest.capabilities = [Capability::ReadMessageHistory].into_iter().collect();
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        let registry = Arc::new(ModuleRegistry::new(vec![module]).unwrap());
        Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()))
    }

    fn message(guild: u64, id: u64) -> ModuleEvent {
        ModuleEvent::MessageCreated(MessageInfo {
            removed_attachments: Vec::new(),
            id: Id::new(id),
            channel_id: Id::new(5),
            guild_id: Id::new(guild),
            author: UserInfo {
                id: Id::new(7),
                name: "someone".into(),
                bot: false,
            },
            author_roles: Vec::new(),
            content: "hello".into(),
            mentioned_users: Vec::new(),
            mentioned_roles: Vec::new(),
            mentions_everyone: false,
            attachments: Vec::new(),
            embed_count: 0,
            sticker_count: 0,
            reply_to: None,
            from_webhook: false,
            created_at: 1_700_000_000,
            edited_at: None,
        })
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn a_message_is_kept_only_where_an_enabled_module_wants_it(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let store = store_with_a_reader();
        store
            .set_guild_yaml(Id::new(2), "modules:\n  reader:\n    enabled: true\n")
            .unwrap();
        let hooks = AppHooks::new(
            db,
            Arc::clone(&store),
            7,
            tokio::sync::mpsc::unbounded_channel().0,
        );

        assert!(!hooks.wants_messages(Id::new(1)));
        assert!(hooks.wants_messages(Id::new(2)));
        hooks.message(&mut message(1, 10)).await;
        hooks.message(&mut message(2, 11)).await;

        let kept: Vec<i64> =
            sqlx::query_scalar("SELECT message_id FROM saved_messages ORDER BY message_id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(kept, [11]);
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn a_new_guild_is_recorded_and_its_discord_language_remembered(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let store = store();
        let hooks = AppHooks::new(
            db,
            Arc::clone(&store),
            7,
            tokio::sync::mpsc::unbounded_channel().0,
        );

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
        let hooks = AppHooks::new(
            db.clone(),
            Arc::clone(&store),
            7,
            tokio::sync::mpsc::unbounded_channel().0,
        );
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
    async fn start_up_reconciliation_adds_joined_and_drops_removed_guilds(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.register_guild(Id::new(1)).await.unwrap();
        db.register_guild(Id::new(2)).await.unwrap();
        db.mark_guild_left(Id::new(2)).await.unwrap();
        db.register_guild(Id::new(3)).await.unwrap();

        // While down: guild 1 stayed, guild 2 was rejoined, guild 3 was removed, guild 4 is new.
        let marked = reconcile_guilds(&db, &[Id::new(1), Id::new(2), Id::new(4)])
            .await
            .unwrap();

        assert_eq!(marked, 1);
        for (guild, active) in [(1, true), (2, true), (3, false), (4, true)] {
            assert_eq!(db.is_guild_active(Id::new(guild)).await.unwrap(), active);
        }
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn database_failures_do_not_panic_or_block(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let hooks = AppHooks::new(db, store(), 7, tokio::sync::mpsc::unbounded_channel().0);
        pool.close().await;
        hooks
            .guild_available(Id::new(5), "Test", Some(Locale::Ja))
            .await;
        hooks.guild_removed(Id::new(5)).await;
    }

    #[test]
    fn a_guild_is_announced_once_per_arrival() {
        let announced = Announced::default();
        let guild = Id::new(5);
        assert!(announced.first(guild));
        assert!(
            !announced.first(guild),
            "a second availability is not announced"
        );
        announced.forget(guild);
        assert!(
            announced.first(guild),
            "a rejoined guild is announced again"
        );
    }
}
