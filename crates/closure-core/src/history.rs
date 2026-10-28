//! Recent messages of a guild, kept so modules can look at what was said before: spam by
//! author, cleaning up a user's messages, logs of edits and deletions.
//!
//! The host records messages, and only for guilds where some enabled module holds the
//! capability `message_history`; where none does, nothing is kept. A module holds a
//! [`MessageHistory`] bound to itself and one guild, so it sees only that guild's messages.
//! Messages older than the bot's retention are removed by [`HistoryBackend::purge_before`].

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::{Capability, CapabilitySet};
use crate::event::{AttachmentInfo, ChannelId, GuildId, MessageId, MessageInfo, RoleId, UserId};
use crate::module::ModuleId;

/// The most messages one listing returns.
pub const MAX_PAGE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    #[error("the module is not granted the capability `message_history`")]
    NotGranted,
    #[error("no message history is available here")]
    Unavailable,
    #[error("the message belongs to another guild")]
    OtherGuild,
    #[error("invalid content: {0}")]
    InvalidContent(String),
    #[error("message history failed: {0}")]
    Backend(String),
}

/// A message as the host kept it. `content` is empty when Discord did not send the text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMessage {
    pub id: MessageId,
    pub channel_id: ChannelId,
    pub guild_id: GuildId,
    pub author: UserId,
    pub author_bot: bool,
    pub content: String,
    pub attachments: Vec<AttachmentInfo>,
    pub embed_count: u32,
    pub sticker_count: u32,
    pub mentioned_users: Vec<UserId>,
    pub mentioned_roles: Vec<RoleId>,
    pub mentions_everyone: bool,
    pub reply_to: Option<MessageId>,
    /// Unix seconds, when the message was sent.
    pub created_at: i64,
    /// Unix seconds of the last edit the host saw.
    pub edited_at: Option<i64>,
    /// Unix seconds, when the host saw the message deleted.
    pub deleted_at: Option<i64>,
}

/// A file kept for an attachment of a message. `stored` is false when the file could not be fetched:
/// the record stays, so that the logs can say so.
#[derive(Debug, Clone, PartialEq)]
pub struct KeptFile {
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    pub attachment: AttachmentInfo,
    pub stored: bool,
    /// Unix seconds, when the file was kept.
    pub kept_at: i64,
}

impl SavedMessage {
    /// The message as it is kept, from the event that carried it.
    #[must_use]
    pub fn from_info(info: &MessageInfo) -> Self {
        Self {
            id: info.id,
            channel_id: info.channel_id,
            guild_id: info.guild_id,
            author: info.author.id,
            author_bot: info.author.bot,
            content: info.content.clone(),
            attachments: info.attachments.clone(),
            embed_count: info.embed_count,
            sticker_count: info.sticker_count,
            mentioned_users: info.mentioned_users.clone(),
            mentioned_roles: info.mentioned_roles.clone(),
            mentions_everyone: info.mentions_everyone,
            reply_to: info.reply_to,
            created_at: info.created_at,
            edited_at: info.edited_at,
            deleted_at: None,
        }
    }
}

/// PostgreSQL cannot keep a NUL character in text (JSON included), so no backend takes one.
pub fn check_text(message: &SavedMessage) -> Result<(), HistoryError> {
    let has_nul = |text: &str| text.contains('\0');
    let attachment_nul = message
        .attachments
        .iter()
        .any(|attachment| has_nul(&attachment.filename) || has_nul(&attachment.url));
    if has_nul(&message.content) || attachment_nul {
        Err(HistoryError::InvalidContent(
            "it must not contain NUL".into(),
        ))
    } else {
        Ok(())
    }
}

/// Where messages are kept. Every call is for one guild; the checks of [`MessageHistory`] come
/// first. Times are unix seconds.
#[async_trait]
pub trait HistoryBackend: Send + Sync {
    /// Stores the message, or replaces the stored one with the same id. A replacement keeps the
    /// time it was sent and the time it was deleted; an empty `content` keeps the stored text,
    /// because Discord sends no text on an update without the content intent.
    async fn save(&self, message: &SavedMessage) -> Result<(), HistoryError>;

    /// Marks the messages deleted at `at`; the first time counts. Unknown ids are ignored.
    async fn mark_deleted(
        &self,
        guild: GuildId,
        ids: &[MessageId],
        at: i64,
    ) -> Result<(), HistoryError>;

    async fn get(
        &self,
        guild: GuildId,
        channel: ChannelId,
        id: MessageId,
    ) -> Result<Option<SavedMessage>, HistoryError>;

    /// The messages of `author` sent at or after `since`, newest first.
    async fn by_author(
        &self,
        guild: GuildId,
        author: UserId,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError>;

    /// The messages of `channel` sent at or after `since`, newest first.
    async fn in_channel(
        &self,
        guild: GuildId,
        channel: ChannelId,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError>;

    /// Removes every message and every file kept before `cutoff`, in every guild; returns how many
    /// messages.
    async fn purge_before(&self, cutoff: i64) -> Result<u64, HistoryError>;

    /// Keeps the file of an attachment of a message. `data` is `None` when the file could not be
    /// fetched. Keeping the same attachment again replaces its file.
    async fn save_file(
        &self,
        guild: GuildId,
        channel: ChannelId,
        message: MessageId,
        attachment: &AttachmentInfo,
        data: Option<&[u8]>,
        at: i64,
    ) -> Result<(), HistoryError>;

    /// The files kept for a message, in the order they were kept.
    async fn files_of(
        &self,
        guild: GuildId,
        message: MessageId,
    ) -> Result<Vec<KeptFile>, HistoryError>;

    /// The bytes of a kept file, when it was stored.
    async fn file_data(
        &self,
        guild: GuildId,
        message: MessageId,
        attachment: u64,
    ) -> Result<Option<Vec<u8>>, HistoryError>;

    /// Drops the oldest stored files of a guild until their bytes come to at most `max_bytes`;
    /// returns how many files were dropped.
    async fn trim_files(&self, guild: GuildId, max_bytes: u64) -> Result<u64, HistoryError>;
}

/// The backend of a [`MessageHistory`] that has none.
struct NoHistory;

#[async_trait]
impl HistoryBackend for NoHistory {
    async fn save(&self, _: &SavedMessage) -> Result<(), HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn mark_deleted(&self, _: GuildId, _: &[MessageId], _: i64) -> Result<(), HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn get(
        &self,
        _: GuildId,
        _: ChannelId,
        _: MessageId,
    ) -> Result<Option<SavedMessage>, HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn by_author(
        &self,
        _: GuildId,
        _: UserId,
        _: i64,
        _: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn in_channel(
        &self,
        _: GuildId,
        _: ChannelId,
        _: i64,
        _: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn purge_before(&self, _: i64) -> Result<u64, HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn save_file(
        &self,
        _: GuildId,
        _: ChannelId,
        _: MessageId,
        _: &AttachmentInfo,
        _: Option<&[u8]>,
        _: i64,
    ) -> Result<(), HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn files_of(&self, _: GuildId, _: MessageId) -> Result<Vec<KeptFile>, HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn file_data(
        &self,
        _: GuildId,
        _: MessageId,
        _: u64,
    ) -> Result<Option<Vec<u8>>, HistoryError> {
        Err(HistoryError::Unavailable)
    }

    async fn trim_files(&self, _: GuildId, _: u64) -> Result<u64, HistoryError> {
        Err(HistoryError::Unavailable)
    }
}

/// A module's way to read (and, for the host, write) the kept messages of one guild.
#[derive(Clone)]
pub struct MessageHistory {
    backend: Arc<dyn HistoryBackend>,
    granted: bool,
    module: ModuleId,
    guild: GuildId,
}

impl fmt::Debug for MessageHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageHistory")
            .field("module", &self.module)
            .field("guild", &self.guild)
            .finish_non_exhaustive()
    }
}

impl MessageHistory {
    /// The history of `guild` for `module`; usable only if `granted` holds
    /// [`Capability::ReadMessageHistory`].
    #[must_use]
    pub fn new(
        backend: Arc<dyn HistoryBackend>,
        granted: &CapabilitySet,
        module: ModuleId,
        guild: GuildId,
    ) -> Self {
        Self {
            backend,
            granted: granted.contains(Capability::ReadMessageHistory),
            module,
            guild,
        }
    }

    /// A history that fails every call, for contexts that keep no messages.
    #[must_use]
    pub fn unavailable(module: ModuleId, guild: GuildId) -> Self {
        Self {
            backend: Arc::new(NoHistory),
            granted: true,
            module,
            guild,
        }
    }

    fn allowed(&self) -> Result<(), HistoryError> {
        if self.granted {
            Ok(())
        } else {
            Err(HistoryError::NotGranted)
        }
    }

    /// Stores a message of this guild (see [`HistoryBackend::save`]).
    pub async fn save(&self, message: &SavedMessage) -> Result<(), HistoryError> {
        self.allowed()?;
        if message.guild_id != self.guild {
            return Err(HistoryError::OtherGuild);
        }
        check_text(message)?;
        self.backend.save(message).await
    }

    /// Marks messages of this guild deleted (see [`HistoryBackend::mark_deleted`]).
    pub async fn mark_deleted(&self, ids: &[MessageId], at: i64) -> Result<(), HistoryError> {
        self.allowed()?;
        self.backend.mark_deleted(self.guild, ids, at).await
    }

    /// The files kept for a message of this guild.
    pub async fn files_of(&self, message: MessageId) -> Result<Vec<KeptFile>, HistoryError> {
        self.allowed()?;
        self.backend.files_of(self.guild, message).await
    }

    /// The bytes of a file kept for a message of this guild, when it was stored.
    pub async fn file_data(
        &self,
        message: MessageId,
        attachment: u64,
    ) -> Result<Option<Vec<u8>>, HistoryError> {
        self.allowed()?;
        self.backend
            .file_data(self.guild, message, attachment)
            .await
    }

    /// The message with `id` in `channel`, if it was kept.
    pub async fn get(
        &self,
        channel: ChannelId,
        id: MessageId,
    ) -> Result<Option<SavedMessage>, HistoryError> {
        self.allowed()?;
        self.backend.get(self.guild, channel, id).await
    }

    /// Up to `limit` (1 to [`MAX_PAGE`]) messages of `author` sent at or after `since`, newest
    /// first. Deleted messages are included; see `deleted_at`.
    pub async fn by_author(
        &self,
        author: UserId,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError> {
        self.allowed()?;
        self.backend
            .by_author(self.guild, author, since, limit.clamp(1, MAX_PAGE))
            .await
    }

    /// Up to `limit` (1 to [`MAX_PAGE`]) messages of `channel` sent at or after `since`, newest
    /// first. Deleted messages are included; see `deleted_at`.
    pub async fn in_channel(
        &self,
        channel: ChannelId,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError> {
        self.allowed()?;
        self.backend
            .in_channel(self.guild, channel, since, limit.clamp(1, MAX_PAGE))
            .await
    }
}

#[derive(Debug, Default)]
struct GuildMessages {
    by_id: HashMap<MessageId, SavedMessage>,
    files: Vec<MemoryFile>,
}

/// A file kept by the memory history, with its bytes when they were fetched.
#[derive(Debug)]
struct MemoryFile {
    kept: KeptFile,
    data: Option<Vec<u8>>,
}

/// [`HistoryBackend`] in memory. For tests and for running without a database.
#[derive(Default)]
pub struct MemoryHistory {
    guilds: Mutex<HashMap<GuildId, GuildMessages>>,
}

impl fmt::Debug for MemoryHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryHistory").finish_non_exhaustive()
    }
}

impl MemoryHistory {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuildId, GuildMessages>> {
        self.guilds.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn newest(
        &self,
        guild: GuildId,
        since: i64,
        limit: usize,
        keep: impl Fn(&SavedMessage) -> bool,
    ) -> Vec<SavedMessage> {
        let guilds = self.lock();
        let Some(messages) = guilds.get(&guild) else {
            return Vec::new();
        };
        let mut found: Vec<&SavedMessage> = messages
            .by_id
            .values()
            .filter(|message| message.created_at >= since && keep(message))
            .collect();
        found.sort_by_key(|message| std::cmp::Reverse((message.created_at, message.id)));
        found.into_iter().take(limit).cloned().collect()
    }
}

#[async_trait]
impl HistoryBackend for MemoryHistory {
    async fn save(&self, message: &SavedMessage) -> Result<(), HistoryError> {
        let mut guilds = self.lock();
        let messages = guilds.entry(message.guild_id).or_default();
        let mut stored = message.clone();
        if let Some(old) = messages.by_id.get(&message.id) {
            stored.created_at = old.created_at;
            stored.deleted_at = old.deleted_at;
            if stored.content.is_empty() {
                stored.content.clone_from(&old.content);
            }
            stored.edited_at = stored.edited_at.or(old.edited_at);
        }
        messages.by_id.insert(stored.id, stored);
        Ok(())
    }

    async fn mark_deleted(
        &self,
        guild: GuildId,
        ids: &[MessageId],
        at: i64,
    ) -> Result<(), HistoryError> {
        let mut guilds = self.lock();
        let Some(messages) = guilds.get_mut(&guild) else {
            return Ok(());
        };
        for id in ids {
            if let Some(message) = messages.by_id.get_mut(id)
                && message.deleted_at.is_none()
            {
                message.deleted_at = Some(at);
            }
        }
        Ok(())
    }

    async fn get(
        &self,
        guild: GuildId,
        channel: ChannelId,
        id: MessageId,
    ) -> Result<Option<SavedMessage>, HistoryError> {
        Ok(self
            .lock()
            .get(&guild)
            .and_then(|messages| messages.by_id.get(&id))
            .filter(|message| message.channel_id == channel)
            .cloned())
    }

    async fn by_author(
        &self,
        guild: GuildId,
        author: UserId,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError> {
        Ok(self.newest(guild, since, limit, |message| message.author == author))
    }

    async fn in_channel(
        &self,
        guild: GuildId,
        channel: ChannelId,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError> {
        Ok(self.newest(guild, since, limit, |message| message.channel_id == channel))
    }

    async fn purge_before(&self, cutoff: i64) -> Result<u64, HistoryError> {
        let mut removed = 0;
        for messages in self.lock().values_mut() {
            let before = messages.by_id.len();
            messages
                .by_id
                .retain(|_, message| message.created_at >= cutoff);
            removed += before - messages.by_id.len();
            messages.files.retain(|file| file.kept.kept_at >= cutoff);
        }
        Ok(u64::try_from(removed).unwrap_or(u64::MAX))
    }

    async fn save_file(
        &self,
        guild: GuildId,
        channel: ChannelId,
        message: MessageId,
        attachment: &AttachmentInfo,
        data: Option<&[u8]>,
        at: i64,
    ) -> Result<(), HistoryError> {
        let mut guilds = self.lock();
        let files = &mut guilds.entry(guild).or_default().files;
        files.retain(|file| {
            !(file.kept.message_id == message && file.kept.attachment.id == attachment.id)
        });
        files.push(MemoryFile {
            kept: KeptFile {
                channel_id: channel,
                message_id: message,
                attachment: attachment.clone(),
                stored: data.is_some(),
                kept_at: at,
            },
            data: data.map(<[u8]>::to_vec),
        });
        Ok(())
    }

    async fn files_of(
        &self,
        guild: GuildId,
        message: MessageId,
    ) -> Result<Vec<KeptFile>, HistoryError> {
        let guilds = self.lock();
        Ok(guilds.get(&guild).map_or_else(Vec::new, |messages| {
            messages
                .files
                .iter()
                .filter(|file| file.kept.message_id == message)
                .map(|file| file.kept.clone())
                .collect()
        }))
    }

    async fn file_data(
        &self,
        guild: GuildId,
        message: MessageId,
        attachment: u64,
    ) -> Result<Option<Vec<u8>>, HistoryError> {
        let guilds = self.lock();
        Ok(guilds
            .get(&guild)
            .and_then(|messages| {
                messages.files.iter().find(|file| {
                    file.kept.message_id == message && file.kept.attachment.id == attachment
                })
            })
            .and_then(|file| file.data.clone()))
    }

    async fn trim_files(&self, guild: GuildId, max_bytes: u64) -> Result<u64, HistoryError> {
        let mut guilds = self.lock();
        let Some(messages) = guilds.get_mut(&guild) else {
            return Ok(0);
        };
        let size = |file: &MemoryFile| {
            file.data
                .as_ref()
                .map_or(0, |data| u64::try_from(data.len()).unwrap_or(u64::MAX))
        };
        let mut total: u64 = messages.files.iter().map(size).sum();
        let mut removed = 0;
        while total > max_bytes {
            let Some(oldest) = messages.files.iter().position(|file| file.data.is_some()) else {
                break;
            };
            total = total.saturating_sub(size(&messages.files[oldest]));
            messages.files.remove(oldest);
            removed += 1;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    fn module() -> ModuleId {
        ModuleId::new("automod").unwrap()
    }

    fn saved(id: u64, channel: u64, author: u64, created_at: i64, content: &str) -> SavedMessage {
        SavedMessage {
            id: Id::new(id),
            channel_id: Id::new(channel),
            guild_id: Id::new(1),
            author: Id::new(author),
            author_bot: false,
            content: content.into(),
            attachments: Vec::new(),
            embed_count: 0,
            sticker_count: 0,
            mentioned_users: Vec::new(),
            mentioned_roles: Vec::new(),
            mentions_everyone: false,
            reply_to: None,
            created_at,
            edited_at: None,
            deleted_at: None,
        }
    }

    fn history(backend: &Arc<MemoryHistory>, guild: u64) -> MessageHistory {
        let granted: CapabilitySet = [Capability::ReadMessageHistory].into_iter().collect();
        MessageHistory::new(
            Arc::clone(backend) as Arc<dyn HistoryBackend>,
            &granted,
            module(),
            Id::new(guild),
        )
    }

    fn ids(messages: &[SavedMessage]) -> Vec<u64> {
        messages.iter().map(|message| message.id.get()).collect()
    }

    #[tokio::test]
    async fn a_saved_message_reads_back_whole() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        let mut message = saved(10, 5, 7, 100, "hello");
        message.attachments = vec![AttachmentInfo {
            id: 3,
            filename: "a.png".into(),
            size: 9,
            content_type: Some("image/png".into()),
            url: "https://cdn.example/a.png".into(),
        }];
        message.mentioned_users = vec![Id::new(8)];
        message.reply_to = Some(Id::new(4));
        history.save(&message).await.unwrap();
        assert_eq!(
            history.get(Id::new(5), Id::new(10)).await.unwrap(),
            Some(message)
        );
    }

    #[tokio::test]
    async fn an_edit_replaces_the_text_and_keeps_when_it_was_sent() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        history.save(&saved(10, 5, 7, 100, "first")).await.unwrap();
        let mut edited = saved(10, 5, 7, 999, "second");
        edited.edited_at = Some(200);
        history.save(&edited).await.unwrap();
        let stored = history.get(Id::new(5), Id::new(10)).await.unwrap().unwrap();
        assert_eq!(stored.content, "second");
        assert_eq!(stored.created_at, 100);
        assert_eq!(stored.edited_at, Some(200));
    }

    #[tokio::test]
    async fn an_update_without_text_keeps_the_stored_text() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        history.save(&saved(10, 5, 7, 100, "kept")).await.unwrap();
        let mut update = saved(10, 5, 7, 100, "");
        update.edited_at = Some(200);
        history.save(&update).await.unwrap();
        let stored = history.get(Id::new(5), Id::new(10)).await.unwrap().unwrap();
        assert_eq!(stored.content, "kept");
        assert_eq!(stored.edited_at, Some(200));
    }

    #[tokio::test]
    async fn a_deletion_is_marked_once_and_keeps_the_text() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        history.save(&saved(10, 5, 7, 100, "bye")).await.unwrap();
        history
            .mark_deleted(&[Id::new(10), Id::new(11)], 150)
            .await
            .unwrap();
        history.mark_deleted(&[Id::new(10)], 160).await.unwrap();
        let stored = history.get(Id::new(5), Id::new(10)).await.unwrap().unwrap();
        assert_eq!(stored.deleted_at, Some(150));
        assert_eq!(stored.content, "bye");
    }

    #[tokio::test]
    async fn get_needs_the_channel_the_message_was_sent_in() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        history.save(&saved(10, 5, 7, 100, "x")).await.unwrap();
        assert_eq!(history.get(Id::new(6), Id::new(10)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn by_author_lists_that_authors_messages_newest_first_from_since() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        for message in [
            saved(1, 5, 7, 100, "a"),
            saved(2, 5, 8, 110, "b"),
            saved(3, 6, 7, 120, "c"),
            saved(4, 5, 7, 130, "d"),
        ] {
            history.save(&message).await.unwrap();
        }
        let all = history.by_author(Id::new(7), 0, 10).await.unwrap();
        assert_eq!(ids(&all), [4, 3, 1]);
        let since = history.by_author(Id::new(7), 110, 10).await.unwrap();
        assert_eq!(ids(&since), [4, 3]);
        let first = history.by_author(Id::new(7), 0, 2).await.unwrap();
        assert_eq!(ids(&first), [4, 3]);
    }

    #[tokio::test]
    async fn in_channel_lists_one_channel_newest_first() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        for message in [
            saved(1, 5, 7, 100, "a"),
            saved(2, 5, 8, 110, "b"),
            saved(3, 6, 7, 120, "c"),
            saved(4, 5, 7, 130, "d"),
        ] {
            history.save(&message).await.unwrap();
        }
        let all = history.in_channel(Id::new(5), 0, 10).await.unwrap();
        assert_eq!(ids(&all), [4, 2, 1]);
        let since = history.in_channel(Id::new(5), 105, 10).await.unwrap();
        assert_eq!(ids(&since), [4, 2]);
        let one = history.in_channel(Id::new(5), 0, 1).await.unwrap();
        assert_eq!(ids(&one), [4]);
    }

    #[tokio::test]
    async fn listings_are_clamped_to_one_through_max_page() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        for id in 1..=150 {
            history
                .save(&saved(id, 5, 7, i64::try_from(id).unwrap(), "x"))
                .await
                .unwrap();
        }
        assert_eq!(history.by_author(Id::new(7), 0, 0).await.unwrap().len(), 1);
        assert_eq!(
            history.in_channel(Id::new(5), 0, 1000).await.unwrap().len(),
            MAX_PAGE
        );
    }

    #[tokio::test]
    async fn purge_removes_only_what_was_sent_before_the_cutoff() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        for message in [
            saved(1, 5, 7, 100, "a"),
            saved(2, 5, 7, 200, "b"),
            saved(3, 5, 7, 300, "c"),
        ] {
            history.save(&message).await.unwrap();
        }
        assert_eq!(backend.purge_before(200).await.unwrap(), 1);
        assert_eq!(history.get(Id::new(5), Id::new(1)).await.unwrap(), None);
        assert!(history.get(Id::new(5), Id::new(2)).await.unwrap().is_some());
        assert!(history.get(Id::new(5), Id::new(3)).await.unwrap().is_some());
        assert_eq!(backend.purge_before(200).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn without_the_capability_every_call_is_refused() {
        let backend = Arc::new(MemoryHistory::default());
        history(&backend, 1)
            .save(&saved(10, 5, 7, 100, "x"))
            .await
            .unwrap();
        let none = MessageHistory::new(
            Arc::clone(&backend) as Arc<dyn HistoryBackend>,
            &CapabilitySet::new(),
            module(),
            Id::new(1),
        );
        assert_eq!(
            none.save(&saved(11, 5, 7, 100, "y")).await,
            Err(HistoryError::NotGranted)
        );
        assert_eq!(
            none.mark_deleted(&[Id::new(10)], 1).await,
            Err(HistoryError::NotGranted)
        );
        assert_eq!(
            none.get(Id::new(5), Id::new(10)).await,
            Err(HistoryError::NotGranted)
        );
        assert_eq!(
            none.by_author(Id::new(7), 0, 10).await,
            Err(HistoryError::NotGranted)
        );
        assert_eq!(
            none.in_channel(Id::new(5), 0, 10).await,
            Err(HistoryError::NotGranted)
        );
    }

    #[tokio::test]
    async fn unavailable_fails_with_unavailable() {
        let history = MessageHistory::unavailable(module(), Id::new(1));
        assert_eq!(
            history.save(&saved(10, 5, 7, 100, "x")).await,
            Err(HistoryError::Unavailable)
        );
        assert_eq!(
            history.get(Id::new(5), Id::new(10)).await,
            Err(HistoryError::Unavailable)
        );
    }

    #[tokio::test]
    async fn guilds_do_not_see_each_others_messages() {
        let backend = Arc::new(MemoryHistory::default());
        history(&backend, 1)
            .save(&saved(10, 5, 7, 100, "x"))
            .await
            .unwrap();
        let elsewhere = history(&backend, 2);
        assert_eq!(elsewhere.get(Id::new(5), Id::new(10)).await.unwrap(), None);
        assert!(
            elsewhere
                .by_author(Id::new(7), 0, 10)
                .await
                .unwrap()
                .is_empty()
        );
        let mut own = saved(11, 5, 7, 100, "y");
        own.guild_id = Id::new(2);
        elsewhere.save(&own).await.unwrap();
        assert_eq!(
            history(&backend, 1)
                .get(Id::new(5), Id::new(11))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_message_of_another_guild_is_refused_by_the_handle() {
        let backend = Arc::new(MemoryHistory::default());
        let mut message = saved(10, 5, 7, 100, "x");
        message.guild_id = Id::new(2);
        assert_eq!(
            history(&backend, 1).save(&message).await,
            Err(HistoryError::OtherGuild)
        );
    }

    #[tokio::test]
    async fn text_with_a_nul_character_is_refused_and_nothing_is_kept() {
        let backend = Arc::new(MemoryHistory::default());
        let history = history(&backend, 1);
        assert!(matches!(
            history.save(&saved(10, 5, 7, 100, "a\0b")).await,
            Err(HistoryError::InvalidContent(_))
        ));
        assert_eq!(history.get(Id::new(5), Id::new(10)).await.unwrap(), None);
    }
}
