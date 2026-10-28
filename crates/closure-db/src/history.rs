//! Messages kept for modules (see `closure_core::history`).

use async_trait::async_trait;
use closure_core::event::{AttachmentInfo, ChannelId, GuildId, MessageId, RoleId, UserId};
use closure_core::history::{HistoryBackend, HistoryError, KeptFile, SavedMessage};

use crate::Db;
use crate::ids::{from_db, to_db};

fn backend(error: impl std::fmt::Display) -> HistoryError {
    HistoryError::Backend(error.to_string())
}

/// A kept file as the database has it: the ids and the times as plain numbers.
#[derive(sqlx::FromRow)]
struct FileRow {
    channel_id: i64,
    message_id: i64,
    attachment_id: i64,
    filename: String,
    size: i64,
    content_type: Option<String>,
    url: String,
    stored: bool,
    kept_at: i64,
}

impl FileRow {
    fn into_kept(self) -> Result<KeptFile, HistoryError> {
        Ok(KeptFile {
            channel_id: from_db(self.channel_id).map_err(backend)?,
            message_id: from_db(self.message_id).map_err(backend)?,
            attachment: AttachmentInfo {
                id: u64::try_from(self.attachment_id).map_err(backend)?,
                filename: self.filename,
                size: u64::try_from(self.size).map_err(backend)?,
                content_type: self.content_type,
                url: self.url,
            },
            stored: self.stored,
            kept_at: self.kept_at,
        })
    }
}

/// The columns of a saved message, with the times as unix seconds. A macro so the queries stay
/// literals.
macro_rules! message_columns {
    () => {
        "guild_id, message_id, channel_id, author_id, author_bot, content, attachments,
         embed_count, sticker_count, mentioned_users, mentioned_roles, mentions_everyone,
         reply_to_id, extract(epoch FROM created_at)::bigint AS created_at,
         extract(epoch FROM edited_at)::bigint AS edited_at,
         extract(epoch FROM deleted_at)::bigint AS deleted_at"
    };
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    guild_id: i64,
    message_id: i64,
    channel_id: i64,
    author_id: i64,
    author_bot: bool,
    content: String,
    attachments: serde_json::Value,
    embed_count: i32,
    sticker_count: i32,
    mentioned_users: Vec<i64>,
    mentioned_roles: Vec<i64>,
    mentions_everyone: bool,
    reply_to_id: Option<i64>,
    created_at: i64,
    edited_at: Option<i64>,
    deleted_at: Option<i64>,
}

impl TryFrom<MessageRow> for SavedMessage {
    type Error = HistoryError;

    fn try_from(row: MessageRow) -> Result<Self, Self::Error> {
        Ok(Self {
            id: from_db(row.message_id).map_err(backend)?,
            channel_id: from_db(row.channel_id).map_err(backend)?,
            guild_id: from_db(row.guild_id).map_err(backend)?,
            author: from_db(row.author_id).map_err(backend)?,
            author_bot: row.author_bot,
            content: row.content,
            attachments: serde_json::from_value(row.attachments).map_err(backend)?,
            embed_count: u32::try_from(row.embed_count).map_err(backend)?,
            sticker_count: u32::try_from(row.sticker_count).map_err(backend)?,
            mentioned_users: row
                .mentioned_users
                .into_iter()
                .map(|id| from_db(id).map_err(backend))
                .collect::<Result<Vec<UserId>, _>>()?,
            mentioned_roles: row
                .mentioned_roles
                .into_iter()
                .map(|id| from_db(id).map_err(backend))
                .collect::<Result<Vec<RoleId>, _>>()?,
            mentions_everyone: row.mentions_everyone,
            reply_to: row
                .reply_to_id
                .map(|id| from_db(id).map_err(backend))
                .transpose()?,
            created_at: row.created_at,
            edited_at: row.edited_at,
            deleted_at: row.deleted_at,
        })
    }
}

fn ids_to_db(ids: &[MessageId]) -> Result<Vec<i64>, HistoryError> {
    ids.iter().map(|id| to_db(*id).map_err(backend)).collect()
}

fn users_to_db(users: &[UserId]) -> Result<Vec<i64>, HistoryError> {
    users.iter().map(|id| to_db(*id).map_err(backend)).collect()
}

fn roles_to_db(roles: &[RoleId]) -> Result<Vec<i64>, HistoryError> {
    roles.iter().map(|id| to_db(*id).map_err(backend)).collect()
}

#[async_trait]
impl HistoryBackend for Db {
    /// An empty `content` keeps the stored text (see the trait), and the time it was sent and
    /// the time it was deleted are never replaced.
    async fn save(&self, message: &SavedMessage) -> Result<(), HistoryError> {
        sqlx::query(
            "INSERT INTO saved_messages (guild_id, message_id, channel_id, author_id, author_bot,
                 content, attachments, embed_count, sticker_count, mentioned_users, mentioned_roles,
                 mentions_everyone, reply_to_id, created_at, edited_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                 to_timestamp($14::bigint), to_timestamp($15::bigint))
             ON CONFLICT (guild_id, message_id) DO UPDATE SET
                 channel_id = EXCLUDED.channel_id,
                 author_id = EXCLUDED.author_id,
                 author_bot = EXCLUDED.author_bot,
                 content = EXCLUDED.content,
                 attachments = EXCLUDED.attachments,
                 embed_count = EXCLUDED.embed_count,
                 sticker_count = EXCLUDED.sticker_count,
                 mentioned_users = EXCLUDED.mentioned_users,
                 mentioned_roles = EXCLUDED.mentioned_roles,
                 mentions_everyone = EXCLUDED.mentions_everyone,
                 reply_to_id = EXCLUDED.reply_to_id,
                 edited_at = COALESCE(EXCLUDED.edited_at, saved_messages.edited_at)",
        )
        .bind(to_db(message.guild_id).map_err(backend)?)
        .bind(to_db(message.id).map_err(backend)?)
        .bind(to_db(message.channel_id).map_err(backend)?)
        .bind(to_db(message.author).map_err(backend)?)
        .bind(message.author_bot)
        .bind(&message.content)
        .bind(serde_json::to_value(&message.attachments).map_err(backend)?)
        .bind(i32::try_from(message.embed_count).map_err(backend)?)
        .bind(i32::try_from(message.sticker_count).map_err(backend)?)
        .bind(users_to_db(&message.mentioned_users)?)
        .bind(roles_to_db(&message.mentioned_roles)?)
        .bind(message.mentions_everyone)
        .bind(
            message
                .reply_to
                .map(|id| to_db(id).map_err(backend))
                .transpose()?,
        )
        .bind(message.created_at)
        .bind(message.edited_at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn mark_deleted(
        &self,
        guild: GuildId,
        ids: &[MessageId],
        at: i64,
    ) -> Result<(), HistoryError> {
        sqlx::query(
            "UPDATE saved_messages SET deleted_at = to_timestamp($3::bigint)
             WHERE guild_id = $1 AND message_id = ANY($2) AND deleted_at IS NULL",
        )
        .bind(to_db(guild).map_err(backend)?)
        .bind(ids_to_db(ids)?)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn get(
        &self,
        guild: GuildId,
        channel: ChannelId,
        id: MessageId,
    ) -> Result<Option<SavedMessage>, HistoryError> {
        let row: Option<MessageRow> = sqlx::query_as(concat!(
            "SELECT ",
            message_columns!(),
            " FROM saved_messages
             WHERE guild_id = $1 AND channel_id = $2 AND message_id = $3"
        ))
        .bind(to_db(guild).map_err(backend)?)
        .bind(to_db(channel).map_err(backend)?)
        .bind(to_db(id).map_err(backend)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        row.map(SavedMessage::try_from).transpose()
    }

    async fn by_author(
        &self,
        guild: GuildId,
        author: UserId,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError> {
        let rows: Vec<MessageRow> = sqlx::query_as(concat!(
            "SELECT ",
            message_columns!(),
            " FROM saved_messages
             WHERE guild_id = $1 AND author_id = $2 AND created_at >= to_timestamp($3::bigint)
             ORDER BY created_at DESC, message_id DESC LIMIT $4"
        ))
        .bind(to_db(guild).map_err(backend)?)
        .bind(to_db(author).map_err(backend)?)
        .bind(since)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(SavedMessage::try_from).collect()
    }

    async fn in_channel(
        &self,
        guild: GuildId,
        channel: ChannelId,
        since: i64,
        limit: usize,
    ) -> Result<Vec<SavedMessage>, HistoryError> {
        let rows: Vec<MessageRow> = sqlx::query_as(concat!(
            "SELECT ",
            message_columns!(),
            " FROM saved_messages
             WHERE guild_id = $1 AND channel_id = $2 AND created_at >= to_timestamp($3::bigint)
             ORDER BY created_at DESC, message_id DESC LIMIT $4"
        ))
        .bind(to_db(guild).map_err(backend)?)
        .bind(to_db(channel).map_err(backend)?)
        .bind(since)
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(SavedMessage::try_from).collect()
    }

    async fn purge_before(&self, cutoff: i64) -> Result<u64, HistoryError> {
        let removed =
            sqlx::query("DELETE FROM saved_messages WHERE created_at < to_timestamp($1::bigint)")
                .bind(cutoff)
                .execute(&self.pool)
                .await
                .map_err(backend)?
                .rows_affected();
        sqlx::query("DELETE FROM message_files WHERE kept_at < to_timestamp($1::bigint)")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(removed)
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
        sqlx::query(
            "INSERT INTO message_files
                (guild_id, message_id, attachment_id, channel_id, filename, size, content_type,
                 url, stored, data, kept_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, to_timestamp($11::bigint))
             ON CONFLICT (guild_id, message_id, attachment_id) DO UPDATE SET
                channel_id = EXCLUDED.channel_id, filename = EXCLUDED.filename,
                size = EXCLUDED.size, content_type = EXCLUDED.content_type, url = EXCLUDED.url,
                stored = EXCLUDED.stored, data = EXCLUDED.data, kept_at = EXCLUDED.kept_at
               WHERE EXCLUDED.stored OR NOT message_files.stored",
        )
        .bind(to_db(guild).map_err(backend)?)
        .bind(to_db(message).map_err(backend)?)
        .bind(i64::try_from(attachment.id).map_err(backend)?)
        .bind(to_db(channel).map_err(backend)?)
        .bind(&attachment.filename)
        .bind(i64::try_from(attachment.size).map_err(backend)?)
        .bind(attachment.content_type.as_deref())
        .bind(&attachment.url)
        .bind(data.is_some())
        .bind(data)
        .bind(at)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn files_of(
        &self,
        guild: GuildId,
        message: MessageId,
    ) -> Result<Vec<KeptFile>, HistoryError> {
        let rows: Vec<FileRow> = sqlx::query_as(
            "SELECT channel_id, message_id, attachment_id, filename, size, content_type, url, stored,
                    extract(epoch FROM kept_at)::bigint AS kept_at
             FROM message_files
             WHERE guild_id = $1 AND message_id = $2
             ORDER BY kept_at, attachment_id",
        )
        .bind(to_db(guild).map_err(backend)?)
        .bind(to_db(message).map_err(backend)?)
        .fetch_all(&self.pool)
        .await
        .map_err(backend)?;
        rows.into_iter().map(FileRow::into_kept).collect()
    }

    async fn file_data(
        &self,
        guild: GuildId,
        message: MessageId,
        attachment: u64,
    ) -> Result<Option<Vec<u8>>, HistoryError> {
        let row: Option<(Option<Vec<u8>>,)> = sqlx::query_as(
            "SELECT data FROM message_files
             WHERE guild_id = $1 AND message_id = $2 AND attachment_id = $3",
        )
        .bind(to_db(guild).map_err(backend)?)
        .bind(to_db(message).map_err(backend)?)
        .bind(i64::try_from(attachment).map_err(backend)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(backend)?;
        Ok(row.and_then(|(data,)| data))
    }

    async fn trim_files(&self, guild: GuildId, max_bytes: u64) -> Result<u64, HistoryError> {
        // A stored file goes when the stored bytes older than it are less than the excess over the
        // limit: the oldest files go first, and the last of them takes the excess past the limit.
        let max = i64::try_from(max_bytes).unwrap_or(i64::MAX);
        let removed = sqlx::query(
            "WITH kept AS (
                SELECT message_id, attachment_id, octet_length(data) AS size,
                       COALESCE(SUM(octet_length(data)) OVER (
                           ORDER BY kept_at, message_id, attachment_id
                           ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING), 0) AS older
                FROM message_files
                WHERE guild_id = $1 AND stored
             ), total AS (
                SELECT COALESCE(SUM(size), 0) AS bytes FROM kept
             )
             DELETE FROM message_files AS files
             USING kept, total
             WHERE files.guild_id = $1
               AND files.message_id = kept.message_id
               AND files.attachment_id = kept.attachment_id
               AND kept.older < total.bytes - $2",
        )
        .bind(to_db(guild).map_err(backend)?)
        .bind(max)
        .execute(&self.pool)
        .await
        .map_err(backend)?
        .rows_affected();
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::capability::{Capability, CapabilitySet};
    use closure_core::event::AttachmentInfo;
    use closure_core::history::{MemoryHistory, MessageHistory};
    use closure_core::module::ModuleId;
    use sqlx::PgPool;
    use twilight_model::id::Id;

    use super::*;

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

    fn history(db: &Db, guild: u64) -> MessageHistory {
        let granted: CapabilitySet = [Capability::ReadMessageHistory].into_iter().collect();
        MessageHistory::new(
            Arc::new(db.clone()) as Arc<dyn HistoryBackend>,
            &granted,
            ModuleId::new("automod").unwrap(),
            Id::new(guild),
        )
    }

    #[sqlx::test]
    async fn a_saved_message_reads_back_whole(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let history = history(&db, 1);
        let mut message = saved(10, 5, 7, 1_700_000_000, "hello");
        message.author_bot = true;
        message.attachments = vec![AttachmentInfo {
            id: 3,
            filename: "a.png".into(),
            size: 9,
            content_type: None,
            url: "https://cdn.example/a.png".into(),
        }];
        message.embed_count = 2;
        message.sticker_count = 1;
        message.mentioned_users = vec![Id::new(8)];
        message.mentioned_roles = vec![Id::new(11)];
        message.mentions_everyone = true;
        message.reply_to = Some(Id::new(4));
        message.edited_at = Some(1_700_000_050);
        history.save(&message).await.unwrap();
        assert_eq!(
            history.get(Id::new(5), Id::new(10)).await.unwrap(),
            Some(message)
        );
    }

    #[sqlx::test]
    async fn an_edit_replaces_the_text_and_an_emptied_text_clears_it(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let history = history(&db, 1);
        history
            .save(&saved(10, 5, 7, 1_700_000_000, "first"))
            .await
            .unwrap();
        let mut edited = saved(10, 5, 7, 1_700_000_999, "second");
        edited.edited_at = Some(1_700_000_100);
        history.save(&edited).await.unwrap();
        let stored = history.get(Id::new(5), Id::new(10)).await.unwrap().unwrap();
        assert_eq!(stored.content, "second");
        assert_eq!(
            stored.created_at, 1_700_000_000,
            "the time it was sent is kept"
        );
        assert_eq!(stored.edited_at, Some(1_700_000_100));

        let mut without_text = saved(10, 5, 7, 1_700_000_000, "");
        without_text.edited_at = Some(1_700_000_200);
        history.save(&without_text).await.unwrap();
        let stored = history.get(Id::new(5), Id::new(10)).await.unwrap().unwrap();
        assert_eq!(stored.content, "");
        assert_eq!(stored.edited_at, Some(1_700_000_200));
    }

    #[sqlx::test]
    async fn an_edit_to_an_empty_text_clears_the_stored_text(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let history = history(&db, 1);
        history
            .save(&saved(10, 5, 7, 1_700_000_000, "abc"))
            .await
            .unwrap();
        let mut emptied = saved(10, 5, 7, 1_700_000_000, "");
        emptied.edited_at = Some(1_700_000_100);
        history.save(&emptied).await.unwrap();
        let stored = history.get(Id::new(5), Id::new(10)).await.unwrap().unwrap();
        assert_eq!(
            stored.content, "",
            "an edit that empties the text clears it"
        );
        assert_eq!(stored.edited_at, Some(1_700_000_100));
    }

    #[sqlx::test]
    async fn a_deletion_is_marked_once_and_unknown_ids_are_ignored(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let history = history(&db, 1);
        history.save(&saved(10, 5, 7, 100, "bye")).await.unwrap();
        history
            .mark_deleted(&[Id::new(10), Id::new(11)], 150)
            .await
            .unwrap();
        history.mark_deleted(&[Id::new(10)], 160).await.unwrap();
        let stored = history.get(Id::new(5), Id::new(10)).await.unwrap().unwrap();
        assert_eq!(stored.deleted_at, Some(150));
        assert_eq!(stored.content, "bye");
        assert_eq!(history.get(Id::new(5), Id::new(11)).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn listings_are_newest_first_filtered_by_since_and_limited(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let history = history(&db, 1);
        for message in [
            saved(1, 5, 7, 100, "a"),
            saved(2, 5, 8, 110, "b"),
            saved(3, 6, 7, 120, "c"),
            saved(4, 5, 7, 130, "d"),
        ] {
            history.save(&message).await.unwrap();
        }
        let ids = |found: Vec<SavedMessage>| -> Vec<u64> {
            found.iter().map(|message| message.id.get()).collect()
        };
        let author = Id::new(7);
        assert_eq!(
            ids(history.by_author(author, 0, 10).await.unwrap()),
            [4, 3, 1]
        );
        assert_eq!(
            ids(history.by_author(author, 110, 10).await.unwrap()),
            [4, 3]
        );
        assert_eq!(ids(history.by_author(author, 0, 2).await.unwrap()), [4, 3]);
        assert_eq!(
            ids(history.in_channel(Id::new(5), 0, 10).await.unwrap()),
            [4, 2, 1]
        );
        assert_eq!(
            ids(history.in_channel(Id::new(5), 105, 10).await.unwrap()),
            [4, 2]
        );
    }

    #[sqlx::test]
    async fn purge_removes_only_what_was_sent_before_the_cutoff(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let history = history(&db, 1);
        for message in [
            saved(1, 5, 7, 100, "a"),
            saved(2, 5, 7, 200, "b"),
            saved(3, 5, 7, 300, "c"),
        ] {
            history.save(&message).await.unwrap();
        }
        assert_eq!(db.purge_before(200).await.unwrap(), 1);
        assert_eq!(history.get(Id::new(5), Id::new(1)).await.unwrap(), None);
        assert!(history.get(Id::new(5), Id::new(2)).await.unwrap().is_some());
        assert_eq!(db.purge_before(200).await.unwrap(), 0);
    }

    #[sqlx::test]
    async fn another_guilds_messages_are_not_visible(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        history(&db, 1)
            .save(&saved(10, 5, 7, 100, "x"))
            .await
            .unwrap();
        let elsewhere = history(&db, 2);
        assert_eq!(elsewhere.get(Id::new(5), Id::new(10)).await.unwrap(), None);
        assert!(
            elsewhere
                .by_author(Id::new(7), 0, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// Runs the same calls on both backends and records the answers, which must match.
    async fn script(backend: Arc<dyn HistoryBackend>) -> Vec<String> {
        let granted: CapabilitySet = [Capability::ReadMessageHistory].into_iter().collect();
        let history = MessageHistory::new(
            backend,
            &granted,
            ModuleId::new("automod").unwrap(),
            Id::new(1),
        );
        let mut log = Vec::new();
        for message in [
            saved(1, 5, 7, 100, "a"),
            saved(2, 5, 8, 110, ""),
            saved(3, 6, 7, 120, "c"),
        ] {
            log.push(format!("{:?}", history.save(&message).await));
        }
        let mut edit = saved(1, 5, 7, 500, "edited");
        edit.edited_at = Some(300);
        log.push(format!("{:?}", history.save(&edit).await));
        log.push(format!(
            "{:?}",
            history.mark_deleted(&[Id::new(2), Id::new(9)], 400).await
        ));
        log.push(format!("{:?}", history.get(Id::new(5), Id::new(1)).await));
        log.push(format!("{:?}", history.by_author(Id::new(7), 0, 10).await));
        log.push(format!("{:?}", history.in_channel(Id::new(5), 0, 1).await));
        log.push(format!(
            "{:?}",
            history.save(&saved(4, 5, 7, 100, "a\0b")).await
        ));
        log
    }

    #[sqlx::test]
    async fn the_database_and_the_memory_backend_agree(pool: PgPool) {
        let db = Db::from_pool(pool);
        db.migrate().await.unwrap();
        let in_memory = script(Arc::new(MemoryHistory::default())).await;
        let in_db = script(Arc::new(db)).await;
        assert_eq!(in_memory, in_db);
    }
}
