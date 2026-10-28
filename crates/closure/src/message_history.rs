//! Keeps the messages modules read (see `closure_core::history`): records them where an enabled
//! module holds `message_history`, updates and marks them deleted, and removes the ones past the
//! retention.

use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use closure_config::SettingsStore;
use closure_core::capability::Capability;
use closure_core::event::{AttachmentInfo, ChannelId, Event, GuildId, MessageId, MessageInfo};
use closure_core::history::{HistoryBackend, HistoryError, SavedMessage, check_text};

use closure_core::attachments::{self, ARRIVAL_FETCHES, Fetcher};

const DAY_SECONDS: i64 = 86_400;

/// How many arrivals may wait for their files at once. Past it an arrival is not kept as it comes;
/// its files are fetched later, when a deletion or an edit needs them.
pub(crate) const BACKLOG_LIMIT: usize = 64;

/// The time now, in Unix seconds.
#[must_use]
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The earliest time a message may have been sent at and still be kept. With no retention
/// (`0` days) that is now, so everything sent before it goes.
#[must_use]
pub fn retention_cutoff(now: i64, retention_days: u32) -> i64 {
    now.saturating_sub(i64::from(retention_days) * DAY_SECONDS)
}

/// Writes the messages of the guilds where they are wanted, and removes the old ones.
pub struct MessageRecorder {
    backend: Arc<dyn HistoryBackend>,
    store: Arc<SettingsStore>,
    retention_days: u32,
    fetcher: Arc<dyn Fetcher>,
    arrivals: Arc<tokio::sync::Semaphore>,
    backlog: Arc<tokio::sync::Semaphore>,
}

impl fmt::Debug for MessageRecorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageRecorder")
            .field("retention_days", &self.retention_days)
            .finish_non_exhaustive()
    }
}

impl MessageRecorder {
    #[must_use]
    pub fn new(
        backend: Arc<dyn HistoryBackend>,
        store: Arc<SettingsStore>,
        retention_days: u32,
        fetcher: Arc<dyn Fetcher>,
    ) -> Self {
        Self {
            backend,
            store,
            retention_days,
            fetcher,
            arrivals: Arc::new(tokio::sync::Semaphore::new(ARRIVAL_FETCHES)),
            backlog: Arc::new(tokio::sync::Semaphore::new(BACKLOG_LIMIT)),
        }
    }

    /// Whether messages of `guild` are kept now: the retention is not zero, and an enabled module
    /// holds the capability in this guild.
    #[must_use]
    pub fn keeps(&self, guild: GuildId) -> bool {
        self.retention_days > 0
            && self
                .store
                .settings(guild)
                .effective
                .modules
                .values()
                .any(|module| {
                    module.enabled && module.capabilities.contains(Capability::ReadMessageHistory)
                })
    }

    /// Applies one gateway event at unix time `now`. Messages are stored only where
    /// [`Self::keeps`] says so; a deletion is marked wherever it happens, since it adds no text.
    /// Other events are ignored.
    pub async fn record(&self, event: &mut Event, now: i64) -> Result<(), HistoryError> {
        let updated = matches!(event, Event::MessageUpdated(_));
        match event {
            Event::MessageCreated(info) | Event::MessageUpdated(info) => {
                if self.keeps(info.guild_id) {
                    let previous = if updated {
                        self.backend
                            .get(info.guild_id, info.channel_id, info.id)
                            .await?
                    } else {
                        None
                    };
                    if let Some(previous) = &previous {
                        // What this edit took away: the attachments before it that it no longer has.
                        let removed: Vec<AttachmentInfo> = previous
                            .attachments
                            .iter()
                            .filter(|old| !info.attachments.iter().any(|new| new.id == old.id))
                            .cloned()
                            .collect();
                        info.removed_attachments = removed;
                    }
                    let message = SavedMessage::from_info(info);
                    check_text(&message)?;
                    self.backend.save(&message).await?;
                    self.keep_files(info, previous.as_ref(), now).await?;
                }
                Ok(())
            }
            Event::MessageDeleted {
                guild_id,
                channel_id,
                message_id,
            } => {
                self.backend
                    .mark_deleted(*guild_id, &[*message_id], now)
                    .await?;
                self.fetch_deleted(*guild_id, *channel_id, &[*message_id], now)
                    .await
            }
            Event::MessagesBulkDeleted {
                guild_id,
                channel_id,
                message_ids,
            } => {
                self.backend
                    .mark_deleted(*guild_id, message_ids, now)
                    .await?;
                self.fetch_deleted(*guild_id, *channel_id, message_ids, now)
                    .await
            }
            _ => Ok(()),
        }
    }

    /// Whether an enabled module of `guild` asks for the files of attachments.
    fn wants_files(&self, guild: GuildId) -> bool {
        self.retention_days > 0
            && self
                .store
                .settings(guild)
                .effective
                .modules
                .values()
                .any(|module| {
                    module.enabled && module.capabilities.contains(Capability::KeepAttachments)
                })
    }

    /// Whether the files of new attachments are kept as their messages arrive: a module that asks for
    /// them is enabled, and its `keep_attachments` setting is on. Off by default.
    fn keeps_files(&self, guild: GuildId) -> bool {
        self.retention_days > 0
            && self
                .store
                .settings(guild)
                .effective
                .modules
                .values()
                .any(|module| {
                    module.enabled
                        && module.capabilities.contains(Capability::KeepAttachments)
                        && module
                            .config
                            .get("keep_attachments")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true)
                })
    }

    /// The files of a message that arrives or is edited. A new attachment is kept in the background
    /// when the setting is on; an attachment the edit removed is fetched now, while Discord may still
    /// serve it.
    async fn keep_files(
        &self,
        info: &MessageInfo,
        previous: Option<&SavedMessage>,
        now: i64,
    ) -> Result<(), HistoryError> {
        if !self.wants_files(info.guild_id) {
            return Ok(());
        }
        if self.keeps_files(info.guild_id) {
            self.keep_in_background(
                info.guild_id,
                info.channel_id,
                info.id,
                info.attachments.clone(),
                now,
            );
        }
        if let Some(previous) = previous {
            let removed: Vec<AttachmentInfo> = previous
                .attachments
                .iter()
                .filter(|old| !info.attachments.iter().any(|new| new.id == old.id))
                .cloned()
                .collect();
            attachments::keep_missing(
                self.backend.as_ref(),
                self.fetcher.as_ref(),
                info.guild_id,
                info.channel_id,
                info.id,
                &removed,
                now,
            )
            .await?;
        }
        Ok(())
    }

    fn keep_in_background(
        &self,
        guild: GuildId,
        channel: ChannelId,
        message: MessageId,
        attachments: Vec<AttachmentInfo>,
        now: i64,
    ) {
        if attachments.is_empty() {
            return;
        }
        // Past the backlog an arrival is not kept as it comes; a deletion or an edit still
        // fetches its files.
        let Ok(backlog) = Arc::clone(&self.backlog).try_acquire_owned() else {
            tracing::warn!(
                %guild,
                %channel,
                %message,
                limit = BACKLOG_LIMIT,
                "too many arrivals wait for their files; this one is not kept as it arrives"
            );
            return;
        };
        let backend = Arc::clone(&self.backend);
        let fetcher = Arc::clone(&self.fetcher);
        let arrivals = Arc::clone(&self.arrivals);
        tokio::spawn(async move {
            // Held until the task ends, so the backlog counts the arrivals that have not finished.
            let _backlog = backlog;
            let Ok(_permit) = arrivals.acquire_owned().await else {
                return;
            };
            if let Err(error) = attachments::keep_missing(
                backend.as_ref(),
                fetcher.as_ref(),
                guild,
                channel,
                message,
                &attachments,
                now,
            )
            .await
            {
                tracing::warn!(%error, "could not keep the attachments of a message");
            }
        });
    }

    /// The files of deleted messages that are not kept yet, fetched now: Discord serves an attachment
    /// only for a while after its message is gone.
    async fn fetch_deleted(
        &self,
        guild: GuildId,
        channel: ChannelId,
        messages: &[MessageId],
        now: i64,
    ) -> Result<(), HistoryError> {
        if !self.wants_files(guild) {
            return Ok(());
        }
        for message in messages {
            if let Some(saved) = self.backend.get(guild, channel, *message).await? {
                attachments::keep_missing(
                    self.backend.as_ref(),
                    self.fetcher.as_ref(),
                    guild,
                    channel,
                    *message,
                    &saved.attachments,
                    now,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Removes the messages sent before the retention allows, at unix time `now`; returns how many.
    pub async fn purge(&self, now: i64) -> Result<u64, HistoryError> {
        self.backend
            .purge_before(retention_cutoff(now, self.retention_days))
            .await
    }
}

#[cfg(test)]
mod tests {
    use closure_config::ConfigCompiler;
    use closure_core::event::{MessageInfo, UserInfo};
    use closure_core::history::MemoryHistory;
    use closure_core::module::{Module, ModuleId, ModuleManifest};
    use closure_core::registry::ModuleRegistry;
    use closure_core::testing::StubModule;
    use twilight_model::id::Id;

    use super::*;

    const NOW: i64 = 1_800_000_000;

    /// A store with one module that declares `message_history`; it is disabled by default.
    fn store() -> Arc<SettingsStore> {
        let mut manifest = ModuleManifest::new(ModuleId::new("reader").unwrap(), "reader", "0.1.0");
        manifest.capabilities = [Capability::ReadMessageHistory].into_iter().collect();
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        let registry = Arc::new(ModuleRegistry::new(vec![module]).unwrap());
        Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()))
    }

    /// Serves nothing: these tests keep no files.
    struct Unreachable;

    #[async_trait::async_trait]
    impl Fetcher for Unreachable {
        async fn fetch(&self, _: &str) -> Result<Vec<u8>, String> {
            Err("offline".into())
        }
    }

    fn recorder(
        store: &Arc<SettingsStore>,
        backend: &Arc<MemoryHistory>,
        days: u32,
    ) -> MessageRecorder {
        MessageRecorder::new(
            Arc::clone(backend) as Arc<dyn HistoryBackend>,
            Arc::clone(store),
            days,
            Arc::new(Unreachable),
        )
    }

    fn info(guild: u64, id: u64, created_at: i64, content: &str) -> MessageInfo {
        MessageInfo {
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
            content: content.into(),
            mentioned_users: Vec::new(),
            mentioned_roles: Vec::new(),
            mentions_everyone: false,
            attachments: Vec::new(),
            embed_count: 0,
            sticker_count: 0,
            reply_to: None,
            from_webhook: false,
            created_at,
            edited_at: None,
        }
    }

    fn created(guild: u64, id: u64, created_at: i64, content: &str) -> Event {
        Event::MessageCreated(info(guild, id, created_at, content))
    }

    #[tokio::test]
    async fn messages_are_kept_only_where_an_enabled_module_holds_the_capability() {
        let store = store();
        let backend = Arc::new(MemoryHistory::default());
        let recorder = recorder(&store, &backend, 7);
        // Guild 1 has no configuration: the module is disabled there by default.
        recorder
            .record(&mut created(1, 10, NOW, "hidden"), NOW)
            .await
            .unwrap();
        assert_eq!(
            backend
                .get(Id::new(1), Id::new(5), Id::new(10))
                .await
                .unwrap(),
            None
        );

        store
            .set_guild_yaml(Id::new(2), "modules:\n  reader:\n    enabled: true\n")
            .unwrap();
        recorder
            .record(&mut created(2, 11, NOW, "kept"), NOW)
            .await
            .unwrap();
        let stored = backend
            .get(Id::new(2), Id::new(5), Id::new(11))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.content, "kept");
        assert_eq!(stored.author, Id::new(7));
    }

    #[tokio::test]
    async fn a_module_without_the_capability_gets_no_messages_kept() {
        let mut manifest = ModuleManifest::new(ModuleId::new("plain").unwrap(), "plain", "0.1.0");
        manifest.default_enabled = true;
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        let registry = Arc::new(ModuleRegistry::new(vec![module]).unwrap());
        let store = Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()));
        let backend = Arc::new(MemoryHistory::default());
        let recorder = recorder(&store, &backend, 7);
        assert!(!recorder.keeps(Id::new(1)));
        recorder
            .record(&mut created(1, 10, NOW, "x"), NOW)
            .await
            .unwrap();
        assert_eq!(
            backend
                .get(Id::new(1), Id::new(5), Id::new(10))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn retention_zero_records_nothing() {
        let store = store();
        store
            .set_guild_yaml(Id::new(2), "modules:\n  reader:\n    enabled: true\n")
            .unwrap();
        let backend = Arc::new(MemoryHistory::default());
        let recorder = recorder(&store, &backend, 0);
        assert!(!recorder.keeps(Id::new(2)));
        recorder
            .record(&mut created(2, 11, NOW, "x"), NOW)
            .await
            .unwrap();
        assert_eq!(
            backend
                .get(Id::new(2), Id::new(5), Id::new(11))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn an_edit_updates_the_text_even_when_it_is_emptied() {
        let store = store();
        store
            .set_guild_yaml(Id::new(2), "modules:\n  reader:\n    enabled: true\n")
            .unwrap();
        let backend = Arc::new(MemoryHistory::default());
        let recorder = recorder(&store, &backend, 7);
        recorder
            .record(&mut created(2, 11, NOW, "first"), NOW)
            .await
            .unwrap();

        let mut edit = info(2, 11, NOW, "second");
        edit.edited_at = Some(NOW + 5);
        recorder
            .record(&mut Event::MessageUpdated(edit), NOW + 5)
            .await
            .unwrap();
        let stored = backend
            .get(Id::new(2), Id::new(5), Id::new(11))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.content, "second");
        assert_eq!(stored.edited_at, Some(NOW + 5));

        // An edit that removes the text leaves no text behind.
        recorder
            .record(&mut Event::MessageUpdated(info(2, 11, NOW, "")), NOW + 6)
            .await
            .unwrap();
        let stored = backend
            .get(Id::new(2), Id::new(5), Id::new(11))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.content, "");
    }

    #[tokio::test]
    async fn deletions_are_marked_for_single_and_bulk_deletes() {
        let store = store();
        store
            .set_guild_yaml(Id::new(2), "modules:\n  reader:\n    enabled: true\n")
            .unwrap();
        let backend = Arc::new(MemoryHistory::default());
        let recorder = recorder(&store, &backend, 7);
        for id in [11, 12, 13] {
            recorder
                .record(&mut created(2, id, NOW, "x"), NOW)
                .await
                .unwrap();
        }
        recorder
            .record(
                &mut Event::MessageDeleted {
                    guild_id: Id::new(2),
                    channel_id: Id::new(5),
                    message_id: Id::new(11),
                },
                NOW + 1,
            )
            .await
            .unwrap();
        recorder
            .record(
                &mut Event::MessagesBulkDeleted {
                    guild_id: Id::new(2),
                    channel_id: Id::new(5),
                    message_ids: vec![Id::new(12)],
                },
                NOW + 2,
            )
            .await
            .unwrap();
        let stored = |id: u64| backend.get(Id::new(2), Id::new(5), Id::new(id));
        assert_eq!(stored(11).await.unwrap().unwrap().deleted_at, Some(NOW + 1));
        assert_eq!(stored(12).await.unwrap().unwrap().deleted_at, Some(NOW + 2));
        assert_eq!(stored(13).await.unwrap().unwrap().deleted_at, None);
    }

    #[tokio::test]
    async fn purge_removes_only_what_is_past_the_retention() {
        let store = store();
        store
            .set_guild_yaml(Id::new(2), "modules:\n  reader:\n    enabled: true\n")
            .unwrap();
        let backend = Arc::new(MemoryHistory::default());
        let recorder = recorder(&store, &backend, 1);
        recorder
            .record(&mut created(2, 11, NOW - DAY_SECONDS - 1, "old"), NOW)
            .await
            .unwrap();
        recorder
            .record(&mut created(2, 12, NOW - DAY_SECONDS + 1, "recent"), NOW)
            .await
            .unwrap();
        assert_eq!(recorder.purge(NOW).await.unwrap(), 1);
        assert_eq!(
            backend
                .get(Id::new(2), Id::new(5), Id::new(11))
                .await
                .unwrap(),
            None
        );
        assert!(
            backend
                .get(Id::new(2), Id::new(5), Id::new(12))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn the_cutoff_is_the_retention_before_now_and_zero_days_is_now() {
        assert_eq!(retention_cutoff(NOW, 7), NOW - 7 * DAY_SECONDS);
        assert_eq!(retention_cutoff(NOW, 0), NOW);
        assert_eq!(retention_cutoff(i64::MIN, u32::MAX), i64::MIN);
    }
}
