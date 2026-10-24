use closure_config::GuildOverrides;
use closure_core::event::{GuildId, UserId};
use futures_util::TryStreamExt;
use sqlx::postgres::PgListener;
use time::OffsetDateTime;

use crate::Db;
use crate::error::DbError;
use crate::ids::{from_db, to_db};

/// The PostgreSQL `NOTIFY` channel announcing configuration changes.
pub const CONFIG_CHANNEL: &str = "closure_guild_config";

const MAX_HISTORY: u32 = 200;

type ConfigRow = (i64, i64, serde_json::Value, Option<i64>, OffsetDateTime);

/// A guild's current configuration as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredConfig {
    pub revision: i64,
    pub overrides: GuildOverrides,
    pub updated_by: Option<UserId>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigHistoryEntry {
    pub revision: i64,
    pub overrides: GuildOverrides,
    pub actor: Option<UserId>,
    pub created_at: OffsetDateTime,
}

fn parse_doc(guild: GuildId, doc: serde_json::Value) -> Result<GuildOverrides, DbError> {
    serde_json::from_value(doc).map_err(|source| DbError::CorruptConfig { guild, source })
}

fn stored_from_row(
    guild: GuildId,
    (_, revision, doc, updated_by, updated_at): ConfigRow,
) -> Result<StoredConfig, DbError> {
    Ok(StoredConfig {
        revision,
        overrides: parse_doc(guild, doc)?,
        updated_by: updated_by.map(from_db).transpose()?,
        updated_at,
    })
}

impl Db {
    /// Saves a guild's configuration and returns its new revision.
    ///
    /// `expected` is the revision the editor started from, or `None` to create the first one.
    /// If someone else saved in between, nothing is written and
    /// [`DbError::RevisionConflict`] reports the current revision. The write, its history entry
    /// and the change notification commit together or not at all.
    pub async fn save_config(
        &self,
        guild: GuildId,
        expected: Option<i64>,
        overrides: &GuildOverrides,
        actor: Option<UserId>,
    ) -> Result<i64, DbError> {
        let guild_id = to_db(guild)?;
        let actor_id = actor.map(to_db).transpose()?;
        let doc = serde_json::to_value(overrides)?;

        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO guilds (id) VALUES ($1) ON CONFLICT (id) DO NOTHING")
            .bind(guild_id)
            .execute(&mut *tx)
            .await?;

        let saved: Option<i64> = match expected {
            None => {
                sqlx::query_scalar(
                    "INSERT INTO guild_config (guild_id, revision, doc, updated_by)
                     VALUES ($1, 1, $2, $3)
                     ON CONFLICT (guild_id) DO NOTHING
                     RETURNING revision",
                )
                .bind(guild_id)
                .bind(&doc)
                .bind(actor_id)
                .fetch_optional(&mut *tx)
                .await?
            }
            Some(revision) => {
                // A concurrent writer holds the row lock; once it commits the revision
                // condition no longer matches and this statement updates nothing.
                sqlx::query_scalar(
                    "UPDATE guild_config
                     SET revision = revision + 1, doc = $2, updated_by = $3, updated_at = now()
                     WHERE guild_id = $1 AND revision = $4
                     RETURNING revision",
                )
                .bind(guild_id)
                .bind(&doc)
                .bind(actor_id)
                .bind(revision)
                .fetch_optional(&mut *tx)
                .await?
            }
        };

        let Some(revision) = saved else {
            let actual =
                sqlx::query_scalar("SELECT revision FROM guild_config WHERE guild_id = $1")
                    .bind(guild_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            return Err(DbError::RevisionConflict { expected, actual });
        };

        sqlx::query(
            "INSERT INTO guild_config_history (guild_id, revision, doc, actor)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(guild_id)
        .bind(revision)
        .bind(&doc)
        .bind(actor_id)
        .execute(&mut *tx)
        .await?;

        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(CONFIG_CHANNEL)
            .bind(format!("{guild_id}:{revision}"))
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(revision)
    }

    /// The current configuration of `guild`, if it has one.
    pub async fn load_config(&self, guild: GuildId) -> Result<Option<StoredConfig>, DbError> {
        let row = sqlx::query_as::<_, ConfigRow>(
            "SELECT guild_id, revision, doc, updated_by, updated_at
             FROM guild_config WHERE guild_id = $1",
        )
        .bind(to_db(guild)?)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| stored_from_row(guild, row)).transpose()
    }

    /// Streams every configuration of guilds the bot is still in to `sink`, without loading
    /// them all into memory. A document that no longer parses is reported to the sink as an
    /// error for that guild and does not stop the others. Returns the number of rows visited.
    pub async fn for_each_config(
        &self,
        mut sink: impl FnMut(GuildId, Result<StoredConfig, DbError>) + Send,
    ) -> Result<usize, DbError> {
        let mut rows = sqlx::query_as::<_, ConfigRow>(
            "SELECT c.guild_id, c.revision, c.doc, c.updated_by, c.updated_at
             FROM guild_config c JOIN guilds g ON g.id = c.guild_id
             WHERE g.left_at IS NULL",
        )
        .fetch(&self.pool);

        let mut visited = 0;
        while let Some(row) = rows.try_next().await? {
            let guild = from_db(row.0)?;
            sink(guild, stored_from_row(guild, row));
            visited += 1;
        }
        Ok(visited)
    }

    /// Past revisions of `guild`, newest first.
    pub async fn config_history(
        &self,
        guild: GuildId,
        limit: u32,
    ) -> Result<Vec<ConfigHistoryEntry>, DbError> {
        let rows = sqlx::query_as::<_, (i64, serde_json::Value, Option<i64>, OffsetDateTime)>(
            "SELECT revision, doc, actor, created_at
             FROM guild_config_history WHERE guild_id = $1
             ORDER BY revision DESC LIMIT $2",
        )
        .bind(to_db(guild)?)
        .bind(i64::from(limit.clamp(1, MAX_HISTORY)))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|(revision, doc, actor, created_at)| {
                Ok(ConfigHistoryEntry {
                    revision,
                    overrides: parse_doc(guild, doc)?,
                    actor: actor.map(from_db).transpose()?,
                    created_at,
                })
            })
            .collect()
    }

    /// Subscribes to configuration changes made by any process.
    pub async fn listen_config(&self) -> Result<ConfigListener, DbError> {
        let mut inner = PgListener::connect_with(&self.pool).await?;
        inner.listen(CONFIG_CHANNEL).await?;
        Ok(ConfigListener { inner })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigEvent {
    Changed {
        guild: GuildId,
        revision: i64,
    },
    /// Notifications may have been missed (the connection dropped, or a payload was
    /// unreadable). Reload every configuration to be safe.
    Resync,
}

/// A stream of [`ConfigEvent`]s. `NOTIFY` is not durable, so a caller must also load all
/// configurations at startup and again whenever it receives [`ConfigEvent::Resync`].
pub struct ConfigListener {
    inner: PgListener,
}

impl std::fmt::Debug for ConfigListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigListener").finish_non_exhaustive()
    }
}

impl ConfigListener {
    /// Waits for the next change. Cancel-safe.
    pub async fn next(&mut self) -> Result<ConfigEvent, DbError> {
        let Some(notification) = self.inner.try_recv().await? else {
            return Ok(ConfigEvent::Resync);
        };
        let parsed = notification
            .payload()
            .split_once(':')
            .and_then(|(guild, revision)| {
                let guild = from_db(guild.parse().ok()?).ok()?;
                Some(ConfigEvent::Changed {
                    guild,
                    revision: revision.parse().ok()?,
                })
            });
        Ok(parsed.unwrap_or_else(|| {
            tracing::warn!("unreadable configuration notification payload");
            ConfigEvent::Resync
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use closure_config::parse_overrides;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

    fn doc(yaml: &str) -> GuildOverrides {
        parse_overrides(yaml).unwrap()
    }

    #[sqlx::test]
    async fn create_then_update_bumps_the_revision_and_keeps_history(pool: PgPool) {
        let db = Db::from_pool(pool);
        let guild = Id::new(1);

        assert_eq!(db.load_config(guild).await.unwrap(), None);
        let first = db
            .save_config(guild, None, &doc("preset: small"), Some(Id::new(9)))
            .await
            .unwrap();
        let second = db
            .save_config(guild, Some(first), &doc("preset: large"), None)
            .await
            .unwrap();
        assert_eq!((first, second), (1, 2));

        let stored = db.load_config(guild).await.unwrap().unwrap();
        assert_eq!(stored.revision, 2);
        assert_eq!(stored.overrides, doc("preset: large"));
        assert_eq!(stored.updated_by, None);

        let history = db.config_history(guild, 10).await.unwrap();
        let revisions: Vec<i64> = history.iter().map(|h| h.revision).collect();
        assert_eq!(revisions, [2, 1]);
        assert_eq!(history[1].overrides, doc("preset: small"));
        assert_eq!(history[1].actor, Some(Id::new(9)));
    }

    #[sqlx::test]
    async fn creating_twice_conflicts_and_changes_nothing(pool: PgPool) {
        let db = Db::from_pool(pool);
        let guild = Id::new(1);
        db.save_config(guild, None, &doc("preset: small"), None)
            .await
            .unwrap();

        let error = db
            .save_config(guild, None, &doc("preset: large"), None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbError::RevisionConflict {
                expected: None,
                actual: Some(1)
            }
        ));
        assert_eq!(
            db.load_config(guild).await.unwrap().unwrap().overrides,
            doc("preset: small")
        );
        assert_eq!(db.config_history(guild, 10).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    async fn a_stale_editor_is_rejected(pool: PgPool) {
        let db = Db::from_pool(pool);
        let guild = Id::new(1);
        let revision = db
            .save_config(guild, None, &doc("preset: small"), None)
            .await
            .unwrap();
        db.save_config(guild, Some(revision), &doc("preset: medium"), None)
            .await
            .unwrap();

        let error = db
            .save_config(guild, Some(revision), &doc("preset: large"), None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbError::RevisionConflict {
                expected: Some(1),
                actual: Some(2)
            }
        ));
        assert_eq!(
            db.load_config(guild).await.unwrap().unwrap().overrides,
            doc("preset: medium")
        );
    }

    #[sqlx::test]
    async fn updating_a_missing_configuration_reports_no_current_revision(pool: PgPool) {
        let db = Db::from_pool(pool);
        let error = db
            .save_config(Id::new(1), Some(1), &doc("preset: small"), None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            DbError::RevisionConflict {
                expected: Some(1),
                actual: None
            }
        ));
    }

    #[sqlx::test]
    async fn concurrent_editors_from_the_same_revision_have_exactly_one_winner(pool: PgPool) {
        let db = Db::from_pool(pool);
        let guild = Id::new(1);
        let base = db
            .save_config(guild, None, &doc("preset: small"), None)
            .await
            .unwrap();

        let (medium, large) = (doc("preset: medium"), doc("preset: large"));
        let (a, b) = tokio::join!(
            db.save_config(guild, Some(base), &medium, None),
            db.save_config(guild, Some(base), &large, None),
        );
        assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count(), 1);
        assert_eq!(db.load_config(guild).await.unwrap().unwrap().revision, 2);
        assert_eq!(db.config_history(guild, 10).await.unwrap().len(), 2);
    }

    #[sqlx::test]
    async fn for_each_config_skips_left_guilds_and_isolates_corrupt_documents(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.save_config(Id::new(1), None, &doc("preset: small"), None)
            .await
            .unwrap();
        db.save_config(Id::new(2), None, &doc("preset: large"), None)
            .await
            .unwrap();
        db.save_config(Id::new(3), None, &doc("language: ja"), None)
            .await
            .unwrap();
        db.mark_guild_left(Id::new(3)).await.unwrap();
        sqlx::query("UPDATE guild_config SET doc = '{\"preset\": 5}' WHERE guild_id = 2")
            .execute(&pool)
            .await
            .unwrap();

        let mut seen = Vec::new();
        let visited = db
            .for_each_config(|guild, result| seen.push((guild.get(), result)))
            .await
            .unwrap();

        assert_eq!(visited, 2);
        seen.sort_by_key(|(guild, _)| *guild);
        assert!(matches!(&seen[0], (1, Ok(stored)) if stored.overrides == doc("preset: small")));
        assert!(matches!(&seen[1], (2, Err(DbError::CorruptConfig { .. }))));
    }

    #[sqlx::test]
    async fn loading_a_corrupt_document_is_an_error_for_that_guild(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.save_config(Id::new(1), None, &doc("preset: small"), None)
            .await
            .unwrap();
        sqlx::query("UPDATE guild_config SET doc = '{\"bogus\": true}'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(matches!(
            db.load_config(Id::new(1)).await,
            Err(DbError::CorruptConfig { .. })
        ));
    }

    #[sqlx::test]
    async fn history_is_limited_and_newest_first(pool: PgPool) {
        let db = Db::from_pool(pool);
        let guild = Id::new(1);
        let mut revision = db
            .save_config(guild, None, &doc("preset: small"), None)
            .await
            .unwrap();
        for _ in 0..3 {
            revision = db
                .save_config(guild, Some(revision), &doc("preset: large"), None)
                .await
                .unwrap();
        }
        let history = db.config_history(guild, 2).await.unwrap();
        assert_eq!(
            history.iter().map(|h| h.revision).collect::<Vec<_>>(),
            [4, 3]
        );
    }

    #[sqlx::test]
    async fn the_database_rejects_non_object_documents(pool: PgPool) {
        let db = Db::from_pool(pool.clone());
        db.register_guild(Id::new(1)).await.unwrap();
        let result =
            sqlx::query("INSERT INTO guild_config (guild_id, revision, doc) VALUES (1, 1, '[]')")
                .execute(&pool)
                .await;
        assert!(result.is_err());
    }

    #[sqlx::test]
    async fn saving_notifies_listeners(pool: PgPool) {
        let db = Db::from_pool(pool);
        let mut listener = db.listen_config().await.unwrap();

        let revision = db
            .save_config(Id::new(77), None, &doc("preset: small"), None)
            .await
            .unwrap();

        let event = tokio::time::timeout(Duration::from_secs(5), listener.next())
            .await
            .expect("notification not delivered")
            .unwrap();
        assert_eq!(
            event,
            ConfigEvent::Changed {
                guild: Id::new(77),
                revision
            }
        );
    }

    #[sqlx::test]
    async fn a_conflicting_save_sends_no_notification(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.save_config(Id::new(1), None, &doc("preset: small"), None)
            .await
            .unwrap();
        let mut listener = db.listen_config().await.unwrap();

        let _ = db
            .save_config(Id::new(1), None, &doc("preset: large"), None)
            .await;

        let outcome = tokio::time::timeout(Duration::from_millis(300), listener.next()).await;
        assert!(outcome.is_err(), "no event expected, got {outcome:?}");
    }
}
