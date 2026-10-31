//! The recorder keeps the files of attachments: fetched when a deletion or an edit needs them, and kept
//! as messages arrive when a module asks for it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use closure_config::{ConfigCompiler, SettingsStore};
use closure_core::attachments::{ARRIVAL_FETCHES, Fetcher, MAX_FILE_BYTES};
use closure_core::capability::Capability;
use closure_core::event::{AttachmentInfo, Event, MessageInfo, UserInfo};
use closure_core::history::{HistoryBackend, MemoryHistory};
use closure_core::module::{Module, ModuleId, ModuleManifest};
use closure_core::registry::ModuleRegistry;
use closure_core::testing::StubModule;
use twilight_model::id::Id;

use crate::message_history::{BACKLOG_LIMIT, MessageRecorder};

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
        removed_attachments: Vec::new(),
        previous_content: None,
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
        .record(
            &mut Event::MessageCreated(message(10, vec![picture(1)])),
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(
        calls(&fetcher),
        0,
        "a link is kept, not its bytes, until the message goes"
    );

    recorder.record(&mut deleted(10), NOW + 60).await.unwrap();

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
        .record(
            &mut Event::MessageCreated(message(11, vec![picture(2)])),
            NOW,
        )
        .await
        .unwrap();

    recorder.record(&mut deleted(11), NOW + 60).await.unwrap();

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
        .record(
            &mut Event::MessageCreated(message(12, vec![picture(3)])),
            NOW,
        )
        .await
        .unwrap();

    recorder
        .record(
            &mut Event::MessageUpdated(message(12, Vec::new())),
            NOW + 60,
        )
        .await
        .unwrap();

    assert_eq!(calls(&fetcher), 1);
    assert_eq!(kept(&backend, 12).await, [(3, true)]);
}

#[tokio::test]
async fn an_edit_hands_on_only_the_pictures_it_took_away() {
    let store = store(true);
    enable(&store, false);
    let backend = Arc::new(MemoryHistory::default());
    let fetcher = Arc::new(Served::default());
    let recorder = recorder(&store, &backend, &fetcher);
    recorder
        .record(
            &mut Event::MessageCreated(message(15, vec![picture(3), picture(4)])),
            NOW,
        )
        .await
        .unwrap();

    let mut removing = Event::MessageUpdated(message(15, vec![picture(4)]));
    recorder.record(&mut removing, NOW + 60).await.unwrap();
    assert_eq!(removed_ids(&removing), [3]);

    let mut text_only = Event::MessageUpdated(message(15, vec![picture(4)]));
    recorder.record(&mut text_only, NOW + 120).await.unwrap();
    assert_eq!(removed_ids(&text_only), Vec::<u64>::new());
}

fn removed_ids(event: &Event) -> Vec<u64> {
    match event {
        Event::MessageUpdated(info) => info
            .removed_attachments
            .iter()
            .map(|file| file.id)
            .collect(),
        _ => panic!("expected an update"),
    }
}

#[tokio::test]
async fn with_the_setting_on_a_picture_is_kept_as_its_message_arrives() {
    let store = store(true);
    enable(&store, true);
    let backend = Arc::new(MemoryHistory::default());
    let fetcher = Arc::new(Served::default());
    let recorder = recorder(&store, &backend, &fetcher);

    recorder
        .record(
            &mut Event::MessageCreated(message(13, vec![picture(4)])),
            NOW,
        )
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
            &mut Event::MessageCreated(message(14, vec![elsewhere, huge])),
            NOW,
        )
        .await
        .unwrap();

    recorder.record(&mut deleted(14), NOW + 60).await.unwrap();

    assert_eq!(
        calls(&fetcher),
        0,
        "nothing off the CDN or too large is fetched"
    );
    assert_eq!(kept(&backend, 14).await, [(5, false), (6, false)]);
}

/// Never answers: a fetch that starts stays in progress for good.
struct Stalled;

#[async_trait]
impl Fetcher for Stalled {
    async fn fetch(&self, _url: &str) -> Result<Vec<u8>, String> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn arrivals_beyond_the_backlog_are_not_queued() {
    let store = store(true);
    enable(&store, true);
    let backend = Arc::new(MemoryHistory::default());
    let recorder = MessageRecorder::new(
        Arc::clone(&backend) as Arc<dyn HistoryBackend>,
        Arc::clone(&store),
        7,
        Arc::new(Stalled) as Arc<dyn Fetcher>,
    );

    for id in 0..200_u64 {
        recorder
            .record(
                &mut Event::MessageCreated(message(100 + id, vec![picture(id)])),
                NOW,
            )
            .await
            .unwrap();
    }

    // Every arrival still waiting for its files holds a clone of the backend. Two more are the
    // test's own handle and the recorder's.
    assert!(
        Arc::strong_count(&backend) <= BACKLOG_LIMIT + 2,
        "{} clones of the backend are alive: more arrivals wait than the backlog allows",
        Arc::strong_count(&backend)
    );
}

/// Serves the bytes after a pause, so a keep is still running when the recorder is finished.
struct Slow;

#[async_trait]
impl Fetcher for Slow {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok(format!("bytes of {url}").into_bytes())
    }
}

/// Serves the bytes after a pause, and records the most fetches that ran at once.
#[derive(Default)]
struct Measured {
    running: AtomicUsize,
    most: AtomicUsize,
}

#[async_trait]
impl Fetcher for Measured {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(running, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(format!("bytes of {url}").into_bytes())
    }
}

#[tokio::test]
async fn a_bulk_deletion_fetches_its_messages_a_few_at_once() {
    let store = store(true);
    enable(&store, false);
    let backend = Arc::new(MemoryHistory::default());
    let fetcher = Arc::new(Measured::default());
    let recorder = MessageRecorder::new(
        Arc::clone(&backend) as Arc<dyn HistoryBackend>,
        Arc::clone(&store),
        7,
        Arc::clone(&fetcher) as Arc<dyn Fetcher>,
    );
    let ids: Vec<u64> = (20..30).collect();
    for id in &ids {
        recorder
            .record(
                &mut Event::MessageCreated(message(*id, vec![picture(*id)])),
                NOW,
            )
            .await
            .unwrap();
    }

    recorder
        .record(
            &mut Event::MessagesBulkDeleted {
                guild_id: Id::new(GUILD),
                channel_id: Id::new(CHANNEL),
                message_ids: ids.iter().map(|id| Id::new(*id)).collect(),
            },
            NOW + 60,
        )
        .await
        .unwrap();

    let most = fetcher.most.load(Ordering::SeqCst);
    assert!(most > 1, "the fetches overlap: {most} at most");
    assert!(most <= ARRIVAL_FETCHES, "{most} fetches ran at once");
    for id in ids {
        assert_eq!(kept(&backend, id).await, [(id, true)]);
    }
}

#[tokio::test]
async fn finish_waits_for_the_pictures_still_being_kept() {
    let store = store(true);
    enable(&store, true);
    let backend = Arc::new(MemoryHistory::default());
    let recorder = MessageRecorder::new(
        Arc::clone(&backend) as Arc<dyn HistoryBackend>,
        Arc::clone(&store),
        7,
        Arc::new(Slow) as Arc<dyn Fetcher>,
    );
    recorder
        .record(
            &mut Event::MessageCreated(message(15, vec![picture(4)])),
            NOW,
        )
        .await
        .unwrap();

    recorder.finish(std::time::Duration::from_secs(5)).await;

    assert_eq!(kept(&backend, 15).await, [(4, true)]);
}

#[tokio::test]
async fn finish_stops_the_pictures_that_do_not_finish_in_time() {
    let store = store(true);
    enable(&store, true);
    let backend = Arc::new(MemoryHistory::default());
    let recorder = MessageRecorder::new(
        Arc::clone(&backend) as Arc<dyn HistoryBackend>,
        Arc::clone(&store),
        7,
        Arc::new(Stalled) as Arc<dyn Fetcher>,
    );
    recorder
        .record(
            &mut Event::MessageCreated(message(16, vec![picture(4)])),
            NOW,
        )
        .await
        .unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        recorder.finish(std::time::Duration::from_millis(10)),
    )
    .await
    .expect("finish returns once the pictures still being kept are stopped");
}
