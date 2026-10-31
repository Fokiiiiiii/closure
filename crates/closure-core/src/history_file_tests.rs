//! Tests of the files the history keeps for the attachments of messages.

use std::sync::Arc;
use std::time::Duration;

use crate::attachments::{Fetcher, GUILD_FILE_BYTES, keep_missing};
use crate::capability::{Capability, CapabilitySet};
use crate::event::{AttachmentInfo, GuildId, MessageId};
use crate::history::{HistoryBackend, HistoryError, MemoryHistory, MessageHistory};
use crate::module::ModuleId;
use tokio::sync::Barrier;
use twilight_model::id::Id;

const GUILD: u64 = 1;
const CHANNEL: u64 = 3;

fn picture(id: u64) -> AttachmentInfo {
    AttachmentInfo {
        id,
        filename: "photo.png".into(),
        size: 2048,
        content_type: Some("image/png".into()),
        url: format!("https://cdn.example/attachments/{id}/photo.png"),
    }
}

fn cdn_picture(id: u64) -> AttachmentInfo {
    AttachmentInfo {
        url: format!("https://cdn.discordapp.com/attachments/{id}/photo.png"),
        ..picture(id)
    }
}

/// Serves the same bytes for every link, as the CDN does.
struct FakeCdn;

#[async_trait::async_trait]
impl Fetcher for FakeCdn {
    async fn fetch(&self, _url: &str) -> Result<Vec<u8>, String> {
        Ok(b"png bytes".to_vec())
    }
}

/// Serves its bytes only once every fetch has started, so the fetches must run at the same time.
struct BarrierCdn(Barrier);

#[async_trait::async_trait]
impl Fetcher for BarrierCdn {
    async fn fetch(&self, _url: &str) -> Result<Vec<u8>, String> {
        self.0.wait().await;
        Ok(b"png bytes".to_vec())
    }
}

fn guild() -> GuildId {
    Id::new(GUILD)
}

fn message(id: u64) -> MessageId {
    Id::new(id)
}

async fn keep(backend: &MemoryHistory, attachment: &AttachmentInfo, data: Option<&[u8]>, at: i64) {
    backend
        .save_file(
            guild(),
            Id::new(CHANNEL),
            message(10),
            attachment,
            data,
            at,
            GUILD_FILE_BYTES,
        )
        .await
        .unwrap();
}

fn granted_history(backend: Arc<MemoryHistory>) -> MessageHistory {
    let granted: CapabilitySet = [Capability::ReadMessageHistory].into_iter().collect();
    MessageHistory::new(backend, &granted, ModuleId::new("logs").unwrap(), guild())
}

#[tokio::test]
async fn a_kept_file_is_listed_for_its_message_and_its_bytes_come_back() {
    let backend = Arc::new(MemoryHistory::default());
    keep(&backend, &picture(1), Some(b"png bytes"), 100).await;
    let history = granted_history(Arc::clone(&backend));

    let files = history.files_of(message(10)).await.unwrap();

    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].stored);
    assert_eq!(files[0].attachment.filename, "photo.png");
    assert_eq!(files[0].kept_at, 100);
    let data = history.file_data(message(10), 1).await.unwrap();
    assert_eq!(data.as_deref(), Some(&b"png bytes"[..]));
    assert!(history.files_of(message(11)).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_file_that_could_not_be_fetched_is_listed_as_not_stored() {
    let backend = Arc::new(MemoryHistory::default());
    keep(&backend, &picture(2), None, 100).await;
    let history = granted_history(Arc::clone(&backend));

    let files = history.files_of(message(10)).await.unwrap();

    assert_eq!(files.len(), 1, "{files:?}");
    assert!(!files[0].stored);
    assert_eq!(history.file_data(message(10), 2).await.unwrap(), None);
}

#[tokio::test]
async fn keeping_the_same_attachment_again_replaces_the_file() {
    let backend = Arc::new(MemoryHistory::default());
    keep(&backend, &picture(3), None, 100).await;
    keep(&backend, &picture(3), Some(b"fetched later"), 200).await;
    let history = granted_history(Arc::clone(&backend));

    let files = history.files_of(message(10)).await.unwrap();

    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].stored);
    assert_eq!(files[0].kept_at, 200);
}

#[tokio::test]
async fn trimming_drops_the_oldest_stored_files_until_the_bytes_fit() {
    let backend = Arc::new(MemoryHistory::default());
    keep(&backend, &picture(4), Some(b"aaaa"), 100).await;
    keep(&backend, &picture(5), Some(b"bbbb"), 200).await;
    keep(&backend, &picture(6), Some(b"cccc"), 300).await;

    let removed = backend.trim_files(guild(), 8).await.unwrap();

    assert_eq!(removed, 1);
    let history = granted_history(Arc::clone(&backend));
    let left: Vec<u64> = history
        .files_of(message(10))
        .await
        .unwrap()
        .iter()
        .map(|file| file.attachment.id)
        .collect();
    assert_eq!(left, [5, 6]);
}

#[tokio::test]
async fn purging_drops_the_files_kept_before_the_cutoff() {
    let backend = Arc::new(MemoryHistory::default());
    keep(&backend, &picture(7), Some(b"old"), 10).await;
    keep(&backend, &picture(8), Some(b"new"), 50).await;

    backend.purge_before(30).await.unwrap();

    let history = granted_history(Arc::clone(&backend));
    let left: Vec<u64> = history
        .files_of(message(10))
        .await
        .unwrap()
        .iter()
        .map(|file| file.attachment.id)
        .collect();
    assert_eq!(left, [8]);
}

#[tokio::test]
async fn a_module_without_the_capability_cannot_read_files() {
    let backend = Arc::new(MemoryHistory::default());
    keep(&backend, &picture(9), Some(b"secret"), 100).await;
    let granted = CapabilitySet::new();
    let history = MessageHistory::new(
        backend.clone(),
        &granted,
        ModuleId::new("nosy").unwrap(),
        guild(),
    );

    assert!(matches!(
        history.files_of(message(10)).await,
        Err(HistoryError::NotGranted)
    ));
    assert!(matches!(
        history.file_data(message(10), 9).await,
        Err(HistoryError::NotGranted)
    ));
}

#[tokio::test]
async fn a_failed_fetch_does_not_replace_a_stored_file() {
    let backend = Arc::new(MemoryHistory::default());
    keep(&backend, &picture(1), Some(b"png bytes"), 100).await;
    keep(&backend, &picture(1), None, 200).await;
    let history = granted_history(Arc::clone(&backend));

    let files = history.files_of(message(10)).await.unwrap();

    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].stored);
    let data = history.file_data(message(10), 1).await.unwrap();
    assert_eq!(data.as_deref(), Some(&b"png bytes"[..]));
}

#[tokio::test]
async fn a_file_that_could_not_be_fetched_is_fetched_again_when_asked() {
    let backend = Arc::new(MemoryHistory::default());
    keep(&backend, &cdn_picture(2), None, 100).await;

    keep_missing(
        &*backend,
        &FakeCdn,
        guild(),
        Id::new(CHANNEL),
        message(10),
        &[cdn_picture(2)],
        200,
    )
    .await
    .unwrap();

    let history = granted_history(Arc::clone(&backend));
    let files = history.files_of(message(10)).await.unwrap();
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].stored);
}

#[tokio::test]
async fn missing_files_are_fetched_at_the_same_time() {
    let backend = Arc::new(MemoryHistory::default());
    let attachments: Vec<AttachmentInfo> = (1..=10).map(cdn_picture).collect();
    let cdn = BarrierCdn(Barrier::new(attachments.len()));

    tokio::time::timeout(
        Duration::from_secs(10),
        keep_missing(
            &*backend,
            &cdn,
            guild(),
            Id::new(CHANNEL),
            message(10),
            &attachments,
            100,
        ),
    )
    .await
    .expect("the fetches waited for each other")
    .unwrap();

    let history = granted_history(Arc::clone(&backend));
    let files = history.files_of(message(10)).await.unwrap();
    assert_eq!(files.len(), attachments.len(), "{files:?}");
    assert!(files.iter().all(|file| file.stored));
}
