//! Keeping the in-memory settings in step with the database.
//!
//! The database is the source of truth. At start-up every stored configuration is compiled and
//! installed; afterwards `LISTEN/NOTIFY` tells every process about edits made by any other one.
//! `NOTIFY` is not durable, so whenever the connection may have dropped notifications everything
//! is reloaded.

use std::sync::Arc;
use std::time::Duration;

use closure_config::SettingsStore;
use closure_core::event::GuildId;
use closure_db::{ConfigEvent, Db, DbError, StoredConfig};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Compiles a stored document and installs it, unless the store already holds a newer revision.
/// A document that no longer validates (say, a module was removed) is reported and skipped, so
/// the guild keeps its previous settings instead of one bad record stopping everyone.
fn apply(store: &SettingsStore, guild: GuildId, stored: &StoredConfig) -> bool {
    match store.compiler().compile(&stored.overrides) {
        Ok(settings) => store.install(guild, stored.revision, settings),
        Err(error) => {
            tracing::warn!(%guild, revision = stored.revision, %error, "stored configuration is no longer valid; keeping the previous settings");
            false
        }
    }
}

/// Loads every configuration. Returns how many were installed.
pub async fn load_all(db: &Db, store: &SettingsStore) -> Result<usize, DbError> {
    let mut installed = 0;
    db.for_each_config(|guild, result| match result {
        Ok(stored) => {
            if apply(store, guild, &stored) {
                installed += 1;
            }
        }
        Err(error) => tracing::warn!(%guild, %error, "skipping an unreadable stored configuration"),
    })
    .await?;
    Ok(installed)
}

/// Reloads one guild, e.g. after a change notification or when the bot rejoins it.
pub async fn reload_one(db: &Db, store: &SettingsStore, guild: GuildId) -> Result<(), DbError> {
    if let Some(stored) = db.load_config(guild).await? {
        apply(store, guild, &stored);
    }
    Ok(())
}

/// Subscribes to changes, loads everything once, and keeps the store current in the background.
///
/// Returns after the first load, so the caller can start accepting events knowing that stored
/// settings are in place. Subscribing happens before loading so no edit can fall in between.
pub async fn start(
    db: Db,
    store: Arc<SettingsStore>,
    shutdown: CancellationToken,
) -> Result<JoinHandle<()>, DbError> {
    let listener = db.listen_config().await?;
    let installed = load_all(&db, &store).await?;
    tracing::info!(installed, "loaded guild configurations");
    Ok(tokio::spawn(follow(db, store, listener, shutdown)))
}

async fn follow(
    db: Db,
    store: Arc<SettingsStore>,
    mut listener: closure_db::ConfigListener,
    shutdown: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            () = shutdown.cancelled() => return,
            event = listener.next() => event,
        };
        let outcome = match event {
            Ok(ConfigEvent::Changed { guild, .. }) => reload_one(&db, &store, guild).await,
            Ok(ConfigEvent::Resync) => load_all(&db, &store).await.map(drop),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            tracing::warn!(%error, "configuration sync failed; reconnecting");
            match reconnect(&db, &shutdown).await {
                Some(fresh) => listener = fresh,
                None => return,
            }
            // Edits made while disconnected produced no notification.
            if let Err(error) = load_all(&db, &store).await {
                tracing::warn!(%error, "could not reload configurations after reconnecting");
            }
        }
    }
}

async fn reconnect(db: &Db, shutdown: &CancellationToken) -> Option<closure_db::ConfigListener> {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return None,
            () = tokio::time::sleep(RECONNECT_DELAY) => {}
        }
        match db.listen_config().await {
            Ok(listener) => return Some(listener),
            Err(error) => tracing::warn!(%error, "could not resubscribe to configuration changes"),
        }
    }
}

#[cfg(test)]
mod tests {
    use closure_config::{ConfigCompiler, parse_overrides};
    use closure_core::module::{Module, ModuleId, ModuleManifest};
    use closure_core::registry::ModuleRegistry;
    use closure_core::settings::SettingsProvider;
    use closure_core::testing::StubModule;
    use serde_json::json;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    fn store() -> Arc<SettingsStore> {
        let mut manifest = ModuleManifest::new(ModuleId::new("warn").unwrap(), "warn", "0.1.0");
        manifest.config_schema = json!({ "type": "object" });
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        let registry = Arc::new(ModuleRegistry::new(vec![module]).unwrap());
        Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()))
    }

    async fn save(db: &Db, guild: u64, yaml: &str) -> i64 {
        db.save_config(Id::new(guild), None, &parse_overrides(yaml).unwrap(), None)
            .await
            .unwrap()
    }

    fn language_of(store: &SettingsStore, guild: u64) -> closure_core::locale::Locale {
        store.locale(Id::new(guild))
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn everything_stored_is_loaded_at_start_up(pool: PgPool) {
        let db = Db::from_pool(pool);
        let store = store();
        save(&db, 1, "language: ja").await;
        save(&db, 2, "language: en").await;

        assert_eq!(load_all(&db, &store).await.unwrap(), 2);
        assert_eq!(language_of(&store, 1), closure_core::locale::Locale::Ja);
        assert_eq!(store.configured_guilds(), 2);
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn one_bad_record_does_not_stop_the_others(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        let store = store();
        save(&db, 1, "language: ja").await;
        save(&db, 2, "language: ja").await;
        save(&db, 3, "language: ja").await;
        // Unreadable JSON for guild 2, and a document that parses but names a missing module.
        sqlx::query("UPDATE guild_config SET doc = '{\"preset\": 5}' WHERE guild_id = 2")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE guild_config SET doc = '{\"modules\": {\"gone\": {}}}' WHERE guild_id = 3",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(load_all(&db, &store).await.unwrap(), 1);
        assert!(store.is_configured(Id::new(1)));
        assert!(!store.is_configured(Id::new(2)));
        assert!(!store.is_configured(Id::new(3)));
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn reloading_one_guild_picks_up_a_newer_revision(pool: PgPool) {
        let db = Db::from_pool(pool);
        let store = store();
        let first = save(&db, 1, "language: ja").await;
        reload_one(&db, &store, Id::new(1)).await.unwrap();
        assert_eq!(language_of(&store, 1), closure_core::locale::Locale::Ja);

        db.save_config(
            Id::new(1),
            Some(first),
            &parse_overrides("language: en").unwrap(),
            None,
        )
        .await
        .unwrap();
        reload_one(&db, &store, Id::new(1)).await.unwrap();
        assert_eq!(language_of(&store, 1), closure_core::locale::Locale::En);
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn edits_made_elsewhere_reach_the_store_through_notifications(pool: PgPool) {
        let db = Db::from_pool(pool);
        let store = store();
        save(&db, 1, "language: ja").await;

        let shutdown = CancellationToken::new();
        let task = start(db.clone(), Arc::clone(&store), shutdown.clone())
            .await
            .unwrap();
        assert_eq!(
            language_of(&store, 1),
            closure_core::locale::Locale::Ja,
            "loaded before start returns"
        );

        // Another process edits an existing guild and configures a new one.
        db.save_config(
            Id::new(1),
            Some(1),
            &parse_overrides("language: en").unwrap(),
            None,
        )
        .await
        .unwrap();
        save(&db, 9, "language: ja").await;

        tokio::time::timeout(Duration::from_secs(5), async {
            while language_of(&store, 1) != closure_core::locale::Locale::En
                || !store.is_configured(Id::new(9))
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the store never saw the change");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the sync task should stop on shutdown")
            .unwrap();
    }
}
