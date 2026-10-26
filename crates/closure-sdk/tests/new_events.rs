//! Role changes, threads and case announcements, as modules receive them through the harness.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::sync::{Arc, Mutex};

use closure_core::cases::{Case, CaseKind};
use closure_core::event::Event;
use closure_core::module::ModuleId;
use closure_sdk::testing::Harness;
use closure_sdk::{EventKind, Flow, Intents, ModuleBuilder, Subscription};
use twilight_model::id::Id;

type Log = Arc<Mutex<Vec<String>>>;

fn ids(values: &[Id<twilight_model::id::marker::RoleMarker>]) -> String {
    let values: Vec<u64> = values.iter().map(|id| id.get()).collect();
    format!("{values:?}")
}

/// What a module logs for an event it is given, one short line each.
fn describe(event: &Event) -> String {
    match event {
        Event::RolesChanged(change) => format!(
            "roles:{}:{}:{}",
            change.user.id,
            ids(&change.added),
            ids(&change.removed)
        ),
        Event::ThreadCreated(thread) => format!("created:{}", thread.id),
        Event::ThreadArchived(thread) => format!("archived:{}", thread.id),
        Event::ThreadUnarchived(thread) => format!("unarchived:{}", thread.id),
        Event::ThreadDeleted { thread_id, .. } => format!("deleted:{thread_id}"),
        Event::CaseCreated(case) => format!("case:{}", case.number),
        other => format!("other:{}", other.kind().as_str()),
    }
}

/// Adds a handler to `builder` that logs what it gets for `subscription`.
fn record(builder: ModuleBuilder, subscription: Subscription, log: &Log) -> ModuleBuilder {
    let log = Arc::clone(log);
    builder.on(subscription, move |_, event| {
        let log = Arc::clone(&log);
        async move {
            log.lock().unwrap().push(describe(&event));
            Ok(Flow::Continue)
        }
    })
}

fn recorder(subscription: Subscription, log: &Log) -> ModuleBuilder {
    record(
        ModuleBuilder::new("recorder", "Recorder", "0.1.0").unwrap(),
        subscription,
        log,
    )
}

#[test]
fn the_new_kinds_need_only_the_intents_discord_sends_them_with() {
    let subscribed = |kind| {
        ModuleBuilder::new("watch", "w", "0.1.0")
            .unwrap()
            .on(Subscription::new(kind), |_, _| async { Ok(Flow::Continue) })
    };
    // Role changes come with the members intent, which is privileged and has to be asked for.
    let error = subscribed(EventKind::RolesChanged)
        .build()
        .unwrap_err()
        .to_string();
    assert!(error.contains("GUILD_MEMBERS"), "{error}");
    subscribed(EventKind::RolesChanged)
        .intents(Intents::GUILD_MEMBERS)
        .build()
        .unwrap();
    // Threads and cases come with the guild events every module gets.
    for kind in [
        EventKind::ThreadCreated,
        EventKind::ThreadArchived,
        EventKind::ThreadUnarchived,
        EventKind::ThreadDeleted,
        EventKind::CaseCreated,
    ] {
        subscribed(kind)
            .build()
            .unwrap_or_else(|error| panic!("{kind:?}: {error}"));
    }
}

#[tokio::test]
async fn a_role_change_reaches_a_module_that_subscribed_and_no_other() {
    let log: Log = Arc::default();
    let module =
        recorder(Subscription::new(EventKind::RolesChanged), &log).intents(Intents::GUILD_MEMBERS);
    let harness = Harness::new(module).unwrap();

    assert_eq!(
        harness.roles_changed(7, &[5, 6], &[9]).await.unwrap(),
        Some(Flow::Continue)
    );
    assert_eq!(*log.lock().unwrap(), ["roles:7:[5, 6]:[9]"]);

    let other: Log = Arc::default();
    let threads_only = recorder(Subscription::new(EventKind::ThreadCreated), &other);
    let harness = Harness::new(threads_only).unwrap();
    assert_eq!(harness.roles_changed(7, &[5], &[]).await.unwrap(), None);
    assert!(other.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_thread_is_filtered_by_its_parent_channel() {
    let log: Log = Arc::default();
    let subscription = Subscription::new(EventKind::ThreadCreated).in_channels([Id::new(3)]);
    let harness = Harness::new(recorder(subscription, &log)).unwrap();

    assert_eq!(
        harness.thread_created(20, 3).await.unwrap(),
        Some(Flow::Continue)
    );
    assert_eq!(harness.thread_created(21, 4).await.unwrap(), None);
    assert_eq!(*log.lock().unwrap(), ["created:20"]);
}

#[tokio::test]
async fn archiving_unarchiving_and_deletion_reach_their_own_subscribers() {
    let log: Log = Arc::default();
    let mut module = ModuleBuilder::new("recorder", "Recorder", "0.1.0").unwrap();
    for kind in [
        EventKind::ThreadArchived,
        EventKind::ThreadUnarchived,
        EventKind::ThreadDeleted,
    ] {
        module = record(module, Subscription::new(kind), &log);
    }
    let harness = Harness::new(module).unwrap();

    assert_eq!(
        harness.thread_archived(20, 3).await.unwrap(),
        Some(Flow::Continue)
    );
    assert_eq!(
        harness.thread_unarchived(20, 3).await.unwrap(),
        Some(Flow::Continue)
    );
    assert_eq!(
        harness.thread_deleted(20, 3).await.unwrap(),
        Some(Flow::Continue)
    );
    assert_eq!(harness.thread_created(21, 3).await.unwrap(), None);
    assert_eq!(
        *log.lock().unwrap(),
        ["archived:20", "unarchived:20", "deleted:20"]
    );
}

#[tokio::test]
async fn a_case_announcement_reaches_a_module_that_subscribed() {
    let log: Log = Arc::default();
    let harness = Harness::new(recorder(Subscription::new(EventKind::CaseCreated), &log)).unwrap();
    let case = Case {
        number: 4,
        guild_id: Id::new(1),
        module: ModuleId::new("other").unwrap(),
        kind: CaseKind::new("warn").unwrap(),
        target: Id::new(7),
        moderator: Id::new(8),
        reason: None,
        created_at: 0,
        expires_at: None,
        revoked: None,
    };

    assert_eq!(
        harness.case_created(case).await.unwrap(),
        Some(Flow::Continue)
    );
    assert_eq!(*log.lock().unwrap(), ["case:4"]);
}
