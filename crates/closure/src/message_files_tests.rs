//! The recorder keeps the files of attachments: fetched when a deletion or an edit needs them, and kept
//! as messages arrive when a module asks for it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use closure_config::{ConfigCompiler, SettingsStore};
use closure_core::attachments::{Fetcher, MAX_FILE_BYTES};
use closure_core::capability::Capability;
use closure_core::event::{AttachmentInfo, Event, MessageInfo, UserInfo};
use closure_core::history::{HistoryBackend, MemoryHistory};
use closure_core::module::{Module, ModuleId, ModuleManifest};
use closure_core::registry::ModuleRegistry;
use closure_core::testing::StubModule;
use twilight_model::id::Id;

use crate::message_history::MessageRecorder;

const GUILD: u64 = 2;
const CHANNEL: u64 = 5;
const NOW: i64 = 1_800_000_000;

/// Serves fixed bytes for every URL and counts the fetches.
#[derive(Default)]
struct Served {
    calls: AtomicUsize,
}

#[async_trait]
impl Fetcher for Served {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("bytes of {url}").into_bytes())
    }
}

fn calls(fetcher: &Served) -> usize {
    fetcher.calls.load(Ordering::SeqCst)
}

/// A store with a `logs` module: it asks for files, when `asks_for_files`; and keeps them as they
/// arrive when its setting says so.
fn store(asks_for_files: bool) -> Arc<SettingsStore> {
    let mut manifest = ModuleManifest::new(ModuleId::new("logs").unwrap(), "logs", "0.1.0");
    manifest.capabilities = [Capability::ReadMessageHistory]
        .into_iter()
        .chain(asks_for_files.then_some(Capability::KeepAttachments))
        .collect();
    let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
    let registry = Arc::new(ModuleRegistry::new(vec![module]).unwrap());
    Arc::new(SettingsStore::new(ConfigCompiler::new(registry).unwrap()))
}

fn enable(store: &SettingsStore, keep: bool) {
    store
        .set_guild_yaml(
            Id::new(GUILD),
            &format!(
                "modules:\n  logs:\n    enabled: true\n    config:\n      keep_attachments: {keep}\n"
            ),
        )
        .unwrap();
}

fn recorder(
    store: &Arc<SettingsStore>,
    backend: &Arc<MemoryHistory>,
    fetcher: &Arc<Served>,
) -> MessageRecorder {
    MessageRecorder::new(
        Arc::clone(backend) as Arc<dyn HistoryBackend>,
        Arc::clone(store),
        7,
        Arc::clone(fetcher) as Arc<dyn Fetcher>,
    )
}

fn picture(id: u64) -> AttachmentInfo {
    AttachmentInfo {
        id,
        filename: "photo.png".into(),
        size: 2048,
        content_type: Some("image/png".into()),
        url: format!("https://cdn.discordapp.com/attachments/{CHANNEL}/{id}/photo.png"),
    }
}

fn message(id: u64, attachments: Vec<AttachmentInfo>) -> MessageInfo {
    MessageInfo {
        id: Id::new(id),
        channel_id: Id::new(CHANNEL),
        guild_id: Id::new(GUILD),
        author: UserInfo {
            id: Id::new(7),
            name: "someone".into(),
            bot: false,
        },
        author_roles: Vec::new(),
        content: "look".into(),
        mentioned_users: Vec::new(),
        mentioned_roles: Vec::new(),
        mentions_everyone: false,
        attachments,
        embed_count: 0,
        sticker_count: 0,
        reply_to: None,
        from_webhook: false,
        created_at: NOW,
        edited_at: None,
    }
}

async fn kept(backend: &MemoryHistory, message: u64) -> Vec<(u64, bool)> {
    backend
        .files_of(Id::new(GUILD), Id::new(message))
        .await
        .unwrap()
        .iter()
        .map(|file| (file.attachment.id, file.stored))
        .collect()
}

fn deleted(message: u64) -> Event {
    Event::MessageDeleted {
        guild_id: Id::new(GUILD),
        channel_id: Id::new(CHANNEL),
        message_id: Id::new(message),
    }
}

#[tokio::test]
async fn a_deletion_fetches_the_pictures_that_were_not_kept() {
    let store = store(true);
    enable(&store, false);
    let backend = Arc::new(MemoryHistory::default());
    let fetcher = Arc::new(Served::default());
    let recorder = recorder(&store, &backend, &fetcher);
    recorder
        .record(&Event::MessageCreated(message(10, vec![picture(1)])), NOW)
        .await
        .unwrap();
    assert_eq!(
        calls(&fetcher),
        0,
        "a link is kept, not its bytes, until the message goes"
    );

    recorder.record(&deleted(10), NOW + 60).await.unwrap();

    assert_eq!(calls(&fetcher), 1);
    assert_eq!(kept(&backend, 10).await, [(1, true)]);
}

#[tokio::test]
async fn without_a_module_that_asks_for_files_nothing_is_fetched() {
    let store = store(false);
    enable(&store, false);
    let backend = Arc::new(MemoryHistory::default());
    let fetcher = Arc::new(Served::default());
    let recorder = recorder(&store, &backend, &fetcher);
    recorder
        .record(&Event::MessageCreated(message(11, vec![picture(2)])), NOW)
        .await
        .unwrap();

    recorder.record(&deleted(11), NOW + 60).await.unwrap();

    assert_eq!(calls(&fetcher), 0);
    assert!(kept(&backend, 11).await.is_empty());
}

#[tokio::test]
async fn a_picture_an_edit_removes_is_fetched_while_the_edit_is_recorded() {
    let store = store(true);
    enable(&store, false);
    let backend = Arc::new(MemoryHistory::default());
    let fetcher = Arc::new(Served::default());
    let recorder = recorder(&store, &backend, &fetcher);
    recorder
        .record(&Event::MessageCreated(message(12, vec![picture(3)])), NOW)
        .await
        .unwrap();

    recorder
        .record(&Event::MessageUpdated(message(12, Vec::new())), NOW + 60)
        .await
        .unwrap();

    assert_eq!(calls(&fetcher), 1);
    assert_eq!(kept(&backend, 12).await, [(3, true)]);
}

#[tokio::test]
async fn with_the_setting_on_a_picture_is_kept_as_its_message_arrives() {
    let store = store(true);
    enable(&store, true);
    let backend = Arc::new(MemoryHistory::default());
    let fetcher = Arc::new(Served::default());
    let recorder = recorder(&store, &backend, &fetcher);

    recorder
        .record(&Event::MessageCreated(message(13, vec![picture(4)])), NOW)
        .await
        .unwrap();
    // The fetch runs in the background; wait for it, with a bound.
    for _ in 0..500 {
        if !kept(&backend, 13).await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    assert_eq!(kept(&backend, 13).await, [(4, true)]);
    assert_eq!(calls(&fetcher), 1);
}

#[tokio::test]
async fn links_off_the_cdn_and_pictures_too_large_are_recorded_without_bytes() {
    let store = store(true);
    enable(&store, false);
    let backend = Arc::new(MemoryHistory::default());
    let fetcher = Arc::new(Served::default());
    let recorder = recorder(&store, &backend, &fetcher);
    let elsewhere = AttachmentInfo {
        id: 5,
        filename: "elsewhere.png".into(),
        size: 10,
        content_type: None,
        url: "https://example.com/elsewhere.png".into(),
    };
    let huge = AttachmentInfo {
        id: 6,
        size: MAX_FILE_BYTES + 1,
        ..picture(6)
    };
    recorder
        .record(
            &Event::MessageCreated(message(14, vec![elsewhere, huge])),
            NOW,
        )
        .await
        .unwrap();

    recorder.record(&deleted(14), NOW + 60).await.unwrap();

    assert_eq!(
        calls(&fetcher),
        0,
        "nothing off the CDN or too large is fetched"
    );
    assert_eq!(kept(&backend, 14).await, [(5, false), (6, false)]);
}
