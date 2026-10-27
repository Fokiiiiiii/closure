//! Keeping the in-memory settings in step with the database.
//!
//! The database is the source of truth. At start-up every stored configuration is compiled and
//! installed; afterwards `LISTEN/NOTIFY` tells every process about edits made by any other one.
//! `NOTIFY` is not durable, so whenever the connection may have dropped notifications everything
//! is reloaded.

use std::sync::Arc;
use std::time::Duration;

use closure_config::{GuildOverrides, SettingsStore};
use closure_core::event::GuildId;
use closure_db::{ConfigEvent, Db, DbError};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Compiles a stored document and installs it, unless the store already holds a newer revision.
/// The store decides whether the install changed anything; `report` only picks between a change
/// the modules must hear about and a load they learn of another way (start-up, a rejoin).
/// A document that no longer validates (say, a module was removed) is reported and skipped, so
/// the guild keeps its previous settings instead of one bad record stopping everyone.
fn apply(
    store: &SettingsStore,
    guild: GuildId,
    revision: i64,
    overrides: &GuildOverrides,
    report: bool,
) -> bool {
    match store.compiler().compile(overrides) {
        Ok(settings) if report => store.install(guild, revision, settings),
        Ok(settings) => store.install_silently(guild, revision, settings),
        Err(error) => {
            tracing::warn!(%guild, revision, %error, "stored configuration is no longer valid; keeping the previous settings");
            false
        }
    }
}

/// Loads every configuration at start-up, which reports no change. Returns how many were installed.
pub async fn load_all(db: &Db, store: &SettingsStore) -> Result<usize, DbError> {
    load(db, store, false).await
}

/// Loads every configuration, reporting each guild whose effective configuration changed when
/// `report` is set. Returns how many were installed.
async fn load(db: &Db, store: &SettingsStore, report: bool) -> Result<usize, DbError> {
    let mut installed = 0;
    db.for_each_config(|guild, result| match result {
        Ok(stored) => {
            if apply(store, guild, stored.revision, &stored.overrides, report) {
                installed += 1;
            }
        }
        Err(error) => tracing::warn!(%guild, %error, "skipping an unreadable stored configuration"),
    })
    .await?;
    Ok(installed)
}

/// Reloads one guild when the bot rejoins it. Nothing is reported: the arrival is announced
/// once by the hooks, after the configuration is back.
pub async fn reload_one(db: &Db, store: &SettingsStore, guild: GuildId) -> Result<(), DbError> {
    reload(db, store, guild, false).await
}

/// Reloads one guild, reporting its change when `report` is set.
async fn reload(
    db: &Db,
    store: &SettingsStore,
    guild: GuildId,
    report: bool,
) -> Result<(), DbError> {
    if let Some(stored) = db.load_config(guild).await? {
        apply(store, guild, stored.revision, &stored.overrides, report);
    }
    Ok(())
}

/// Subscribes to changes, loads everything once, and keeps the store current in the background.
///
/// Returns after the first load, so the caller can start accepting events knowing that stored
/// settings are in place. Subscribing happens before loading so no edit can fall in between.
///
/// Every later change of a guild's effective configuration is reported through the store's
/// change hook.
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
        // The listener is moved in and gone when this returns, which hands its connection back
        // to the pool before `resync` needs connections of its own.
        if !apply_notifications(&db, &store, listener, &shutdown).await {
            return;
        }
        match resync(&db, &store, &shutdown).await {
            Some(fresh) => listener = fresh,
            None => return,
        }
    }
}

/// Applies change notifications until one cannot be handled (`true`) or shutdown (`false`).
async fn apply_notifications(
    db: &Db,
    store: &SettingsStore,
    mut listener: closure_db::ConfigListener,
    shutdown: &CancellationToken,
) -> bool {
    loop {
        let event = tokio::select! {
            () = shutdown.cancelled() => return false,
            event = listener.next() => event,
        };
        let outcome = match event {
            Ok(ConfigEvent::Changed { guild, .. }) => reload(db, store, guild, true).await,
            Ok(ConfigEvent::Resync) => load(db, store, true).await.map(drop),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            tracing::warn!(%error, "configuration sync failed; reconnecting");
            return true;
        }
    }
}

/// Resubscribes and reloads everything, retrying until both succeed: edits made while
/// disconnected produced no notification, so a reload that failed once must not be dropped or
/// the store would stay stale until the next unrelated edit. Subscribing comes first so no edit
/// can fall between the two.
async fn resync(
    db: &Db,
    store: &SettingsStore,
    shutdown: &CancellationToken,
) -> Option<closure_db::ConfigListener> {
    retry_until_ok(RECONNECT_DELAY, shutdown, || async {
        let listener = db.listen_config().await?;
        load(db, store, true).await?;
        Ok::<_, DbError>(listener)
    })
    .await
}

/// Runs `attempt` every `delay` until it succeeds. `None` means shutdown came first, which also
/// abandons an attempt in progress.
async fn retry_until_ok<T, E, F, Fut>(
    delay: Duration,
    shutdown: &CancellationToken,
    mut attempt: F,
) -> Option<T>
where
    E: std::fmt::Display,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return None,
            () = tokio::time::sleep(delay) => {}
        }
        // An attempt can hang too (a database that stops answering), and must not hold up shutdown.
        let result = tokio::select! {
            () = shutdown.cancelled() => return None,
            result = attempt() => result,
        };
        match result {
            Ok(value) => return Some(value),
            Err(error) => {
                tracing::warn!(%error, "could not resynchronise configurations; retrying");
            }
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
    use tokio::sync::mpsc::UnboundedReceiver;
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

    #[tokio::test]
    async fn a_failing_resync_is_retried_until_it_succeeds() {
        let mut calls = 0;
        let value = retry_until_ok(Duration::from_millis(5), &CancellationToken::new(), || {
            calls += 1;
            let ok = calls == 3;
            async move { if ok { Ok(7) } else { Err("down") } }
        })
        .await;
        assert_eq!(value, Some(7));
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn retrying_stops_on_shutdown() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let value = retry_until_ok(Duration::from_millis(5), &shutdown, || async {
            Err::<(), _>("down")
        })
        .await;
        assert_eq!(value, None);
    }

    /// A store whose change hook sends every reported guild to the returned receiver.
    fn store_with_reports() -> (Arc<SettingsStore>, UnboundedReceiver<GuildId>) {
        let store = store();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        store.set_change_hook(move |guild| {
            let _ = tx.send(guild);
        });
        (store, rx)
    }

    /// What the admin API does on a save: it writes the document, then installs it.
    async fn api_save(db: &Db, store: &SettingsStore, guild: GuildId, yaml: &str) -> i64 {
        let overrides = parse_overrides(yaml).unwrap();
        let revision = db.save_config(guild, None, &overrides, None).await.unwrap();
        let settings = store.compiler().compile(&overrides).unwrap();
        assert!(store.install(guild, revision, settings));
        revision
    }

    #[test]
    fn a_document_is_reported_only_when_its_effective_config_changes() {
        let (store, mut reports) = store_with_reports();
        let guild = Id::new(5);
        let doc = |yaml: &str| parse_overrides(yaml).unwrap();
        // An empty document is the defaults already, so installing it changes nothing.
        assert!(apply(&store, guild, 1, &doc(""), true));
        assert!(apply(&store, guild, 2, &doc(""), true));
        assert!(reports.try_recv().is_err());
        assert!(apply(&store, guild, 3, &doc("prefix: '?'"), true));
        assert_eq!(reports.try_recv().ok(), Some(guild));
        assert!(
            !apply(&store, guild, 2, &doc(""), true),
            "an older revision is kept out"
        );
        assert!(reports.try_recv().is_err());
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn an_api_save_is_reported_once_and_the_reload_after_it_adds_nothing(pool: PgPool) {
        let db = Db::from_pool(pool);
        let (store, mut reports) = store_with_reports();
        let guild = Id::new(1);
        api_save(&db, &store, guild, "language: ja").await;
        assert_eq!(reports.try_recv().ok(), Some(guild), "the save is a change");

        // The notification that the save sent arrives afterwards.
        reload(&db, &store, guild, true).await.unwrap();
        assert!(reports.try_recv().is_err(), "the reload reported it again");
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn a_reload_before_the_api_install_reports_once(pool: PgPool) {
        let db = Db::from_pool(pool);
        let (store, mut reports) = store_with_reports();
        let guild = Id::new(1);
        let overrides = parse_overrides("language: ja").unwrap();
        let revision = db.save_config(guild, None, &overrides, None).await.unwrap();

        reload(&db, &store, guild, true).await.unwrap();
        assert_eq!(reports.try_recv().ok(), Some(guild));

        // The save path installs the same revision once the reload has already done so.
        let settings = store.compiler().compile(&overrides).unwrap();
        assert!(store.install(guild, revision, settings));
        assert!(reports.try_recv().is_err(), "the install reported it again");
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn each_later_edit_is_reported_once(pool: PgPool) {
        let db = Db::from_pool(pool);
        let (store, mut reports) = store_with_reports();
        let guild = Id::new(1);
        let first = save(&db, 1, "language: ja").await;
        reload(&db, &store, guild, true).await.unwrap();
        assert_eq!(reports.try_recv().ok(), Some(guild));

        let edit = parse_overrides("language: en").unwrap();
        db.save_config(guild, Some(first), &edit, None)
            .await
            .unwrap();
        reload(&db, &store, guild, true).await.unwrap();
        reload(&db, &store, guild, true).await.unwrap();
        assert_eq!(reports.try_recv().ok(), Some(guild));
        assert!(
            reports.try_recv().is_err(),
            "a duplicate reload reported again"
        );
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn start_up_and_rejoin_loads_are_not_reported(pool: PgPool) {
        let db = Db::from_pool(pool);
        let (store, mut reports) = store_with_reports();
        save(&db, 1, "language: ja").await;
        assert_eq!(load_all(&db, &store).await.unwrap(), 1);
        reload_one(&db, &store, Id::new(1)).await.unwrap();
        assert!(reports.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_attempt_that_hangs_does_not_hold_up_shutdown() {
        let shutdown = CancellationToken::new();
        let started = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let (shutdown, started) = (shutdown.clone(), Arc::clone(&started));
            async move {
                retry_until_ok(Duration::from_millis(5), &shutdown, || {
                    started.notify_one();
                    std::future::pending::<Result<(), &str>>()
                })
                .await
            }
        });

        started.notified().await;
        shutdown.cancel();

        let value = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("shutdown must abandon the running attempt")
            .unwrap();
        assert_eq!(value, None);
    }

    #[sqlx::test(migrations = "../closure-db/migrations")]
    async fn resync_recovers_on_a_pool_of_only_two_connections(
        options: sqlx::postgres::PgPoolOptions,
        connect: sqlx::postgres::PgConnectOptions,
    ) {
        // One connection for the listener, one for queries: the smallest working pool.
        let app = Db::from_pool(
            options
                .clone()
                .max_connections(2)
                .acquire_timeout(Duration::from_millis(500))
                .connect_with(connect.clone())
                .await
                .unwrap(),
        );
        let admin = sqlx::PgPool::connect_with(connect).await.unwrap();
        let writer = Db::from_pool(admin.clone());
        let store = store();
        save(&writer, 1, "language: ja").await;

        let shutdown = CancellationToken::new();
        let task = start(app, Arc::clone(&store), shutdown.clone())
            .await
            .unwrap();

        // A notification whose reload fails (the stored document is unreadable) sends the task
        // into a resync while its old listener is still alive.
        sqlx::query("UPDATE guild_config SET doc = '{\"preset\": 5}' WHERE guild_id = 1")
            .execute(&admin)
            .await
            .unwrap();
        sqlx::query("SELECT pg_notify($1, '1:99')")
            .bind(closure_db::CONFIG_CHANNEL)
            .execute(&admin)
            .await
            .unwrap();

        // Once the task has recovered it listens again and sees later edits.
        save(&writer, 9, "language: ja").await;
        tokio::time::timeout(Duration::from_secs(20), async {
            while !store.is_configured(Id::new(9)) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("the sync task never recovered");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the sync task should stop on shutdown")
            .unwrap();
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
