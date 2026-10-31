//! The files the database keeps for the attachments of messages (see `closure_core::history`).

use std::sync::Arc;

use closure_core::capability::{Capability, CapabilitySet};
use closure_core::event::AttachmentInfo;
use closure_core::history::{HistoryBackend, MessageHistory};
use closure_core::module::ModuleId;
use sqlx::PgPool;
use twilight_model::id::Id;

use crate::Db;

const GUILD: u64 = 1;
const CHANNEL: u64 = 3;
const MESSAGE: u64 = 10;

fn picture(id: u64) -> AttachmentInfo {
    AttachmentInfo {
        id,
        filename: "photo.png".into(),
        size: 2048,
        content_type: Some("image/png".into()),
        url: format!("https://cdn.example/attachments/{id}/photo.png"),
    }
}

fn history(db: &Db) -> MessageHistory {
    let granted: CapabilitySet = [Capability::ReadMessageHistory].into_iter().collect();
    MessageHistory::new(
        Arc::new(db.clone()) as Arc<dyn HistoryBackend>,
        &granted,
        ModuleId::new("logs").unwrap(),
        Id::new(GUILD),
    )
}

async fn keep(db: &Db, attachment: &AttachmentInfo, data: Option<&[u8]>, at: i64) {
    db.save_file(
        Id::new(GUILD),
        Id::new(CHANNEL),
        Id::new(MESSAGE),
        attachment,
        data,
        at,
        closure_core::attachments::GUILD_FILE_BYTES,
    )
    .await
    .unwrap();
}

async fn attachment_ids(db: &Db) -> Vec<u64> {
    history(db)
        .files_of(Id::new(MESSAGE))
        .await
        .unwrap()
        .iter()
        .map(|file| file.attachment.id)
        .collect()
}

#[sqlx::test]
async fn a_kept_file_is_listed_and_its_bytes_come_back(pool: PgPool) {
    let db = Db::from_pool(pool);
    db.migrate().await.unwrap();
    keep(&db, &picture(1), Some(b"png bytes"), 1_700_000_000).await;
    keep(&db, &picture(2), None, 1_700_000_001).await;

    let files = history(&db).files_of(Id::new(MESSAGE)).await.unwrap();

    assert_eq!(files.len(), 2, "{files:?}");
    assert!(files[0].stored);
    assert!(!files[1].stored);
    assert_eq!(files[0].attachment.filename, "photo.png");
    assert_eq!(
        files[0].attachment.content_type.as_deref(),
        Some("image/png")
    );
    assert_eq!(files[0].kept_at, 1_700_000_000);
    let data = history(&db).file_data(Id::new(MESSAGE), 1).await.unwrap();
    assert_eq!(data.as_deref(), Some(&b"png bytes"[..]));
    assert_eq!(
        history(&db).file_data(Id::new(MESSAGE), 2).await.unwrap(),
        None
    );
}

#[sqlx::test]
async fn keeping_the_same_attachment_again_replaces_the_file(pool: PgPool) {
    let db = Db::from_pool(pool);
    db.migrate().await.unwrap();
    keep(&db, &picture(3), None, 1_700_000_000).await;
    keep(&db, &picture(3), Some(b"fetched later"), 1_700_000_200).await;

    let files = history(&db).files_of(Id::new(MESSAGE)).await.unwrap();

    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].stored);
    assert_eq!(files[0].kept_at, 1_700_000_200);
}

#[sqlx::test]
async fn trimming_drops_the_oldest_stored_files_until_the_bytes_fit(pool: PgPool) {
    let db = Db::from_pool(pool);
    db.migrate().await.unwrap();
    keep(&db, &picture(4), Some(b"aaaa"), 1_700_000_100).await;
    keep(&db, &picture(5), Some(b"bbbb"), 1_700_000_200).await;
    keep(&db, &picture(6), Some(b"cccc"), 1_700_000_300).await;

    let removed = db.trim_files(Id::new(GUILD), 8).await.unwrap();

    assert_eq!(removed, 1);
    assert_eq!(attachment_ids(&db).await, [5, 6]);
}

#[sqlx::test]
async fn a_save_past_the_limit_drops_the_oldest_at_once(pool: PgPool) {
    let db = Db::from_pool(pool);
    db.migrate().await.unwrap();
    for (id, data, at) in [
        (4, &b"aaaaaa"[..], 1_700_000_100),
        (5, &b"bbbbbb"[..], 1_700_000_200),
    ] {
        db.save_file(
            Id::new(GUILD),
            Id::new(CHANNEL),
            Id::new(MESSAGE),
            &picture(id),
            Some(data),
            at,
            8,
        )
        .await
        .unwrap();
    }

    // The second save kept the limit by itself: a trim afterwards has nothing left to drop.
    assert_eq!(db.trim_files(Id::new(GUILD), 8).await.unwrap(), 0);
    assert_eq!(attachment_ids(&db).await, [5]);
}

#[sqlx::test]
async fn concurrent_saves_leave_the_guild_within_its_limit(pool: PgPool) {
    let db = Db::from_pool(pool);
    db.migrate().await.unwrap();
    let saves: Vec<_> = (1..=8_u64)
        .map(|id| {
            let db = db.clone();
            tokio::spawn(async move {
                let at = 1_700_000_000 + i64::try_from(id).unwrap();
                db.save_file(
                    Id::new(GUILD),
                    Id::new(CHANNEL),
                    Id::new(MESSAGE),
                    &picture(id),
                    Some(b"abc"),
                    at,
                    10,
                )
                .await
            })
        })
        .collect();
    for save in saves {
        save.await.unwrap().unwrap();
    }

    // Three files of three bytes fit in ten: the five oldest went when the limit was passed.
    let kept: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(octet_length(data)), 0)::bigint FROM message_files WHERE guild_id = $1",
    )
    .bind(i64::try_from(GUILD).unwrap())
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(kept, 9);
}

#[sqlx::test]
async fn purging_drops_the_files_kept_before_the_cutoff(pool: PgPool) {
    let db = Db::from_pool(pool);
    db.migrate().await.unwrap();
    keep(&db, &picture(7), Some(b"old"), 1_700_000_000).await;
    keep(&db, &picture(8), Some(b"new"), 1_700_000_500).await;

    db.purge_before(1_700_000_100).await.unwrap();

    assert_eq!(attachment_ids(&db).await, [8]);
}

#[sqlx::test]
async fn a_failed_fetch_does_not_replace_a_stored_file(pool: PgPool) {
    let db = Db::from_pool(pool);
    db.migrate().await.unwrap();
    keep(&db, &picture(1), Some(b"png bytes"), 1_700_000_000).await;
    keep(&db, &picture(1), None, 1_700_000_200).await;

    let files = history(&db).files_of(Id::new(MESSAGE)).await.unwrap();

    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].stored);
    let data = history(&db).file_data(Id::new(MESSAGE), 1).await.unwrap();
    assert_eq!(data.as_deref(), Some(&b"png bytes"[..]));
}
