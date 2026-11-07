#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::action::{Action, Outcome};
use closure_core::capability::CapabilitySet;
use closure_core::client::{ApiClient, DiscordApi};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, GuildId};
use closure_core::module::Module;
use closure_core::schedule::ScheduledJob;
use closure_core::testing::MockApi;
use closure_sdk::punish::{self, Method, Moderator, Punisher, Settings};
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Capability, Command, CommandCx, EventContext, ModuleBuilder, UserError};
use serde_json::json;
use tokio::sync::Notify;
use twilight_model::id::Id;

static CONTEXT: Mutex<Option<EventContext>> = Mutex::new(None);
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn capture(cx: CommandCx, (): ()) -> Result<(), UserError> {
    *CONTEXT.lock().unwrap() = Some(cx.ctx().clone());
    Ok(())
}

struct PausedApi {
    inner: Arc<MockApi>,
    armed: AtomicBool,
    held: AtomicBool,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl DiscordApi for PausedApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        let lift = matches!(action, Action::RemoveRole { .. } | Action::Unban { .. });
        if lift && self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        let give = matches!(action, Action::AddRole { .. } | Action::Ban { .. });
        let result = self.inner.execute(action).await;
        if result.is_ok() {
            if lift {
                self.held.store(false, Ordering::SeqCst);
            }
            if give {
                self.held.store(true, Ordering::SeqCst);
            }
        }
        result
    }

    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        self.inner.channel_guild(channel).await
    }
}

struct PausedLog {
    inner: Arc<MockApi>,
    entered: Notify,
}

#[async_trait]
impl DiscordApi for PausedLog {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        if matches!(action, Action::SendMessage { .. }) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.execute(action).await
    }

    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        self.inner.channel_guild(channel).await
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Timeout,
    Ban,
    Voice,
}

async fn punish(ctx: &EventContext, kind: Kind) -> i64 {
    let settings = Settings {
        dm: false,
        timeout: Method::Role,
        timeout_role: Some(Id::new(30)),
        voice: Method::Role,
        voice_role: Some(Id::new(30)),
        ..Settings::default()
    };
    let moderator = Moderator::bot(ctx, "test").unwrap();
    let punisher = Punisher::new(ctx, moderator, settings);
    let duration = Duration::from_secs(3600);
    match kind {
        Kind::Timeout => punisher.timeout(Id::new(7), duration, None).await,
        Kind::Ban => punisher.ban(Id::new(7), Some(duration), 0, None).await,
        Kind::Voice => punisher.voice_mute(Id::new(7), Some(duration), None).await,
    }
    .unwrap()
    .case
    .number
}

async fn timer_survives_aborted_log(kind: Kind) {
    let _guard = TEST_LOCK.lock().await;
    let mut builder = ModuleBuilder::new("race", "Race", "1.0.0").unwrap();
    for capability in Capability::ALL {
        builder = builder.capability(capability);
    }
    let builder = punish::jobs(
        builder
            .config(json!({ "type": "object", "properties": {} }), json!({}))
            .command(Command::<()>::new("capture").handler(capture)),
    );
    let harness = Harness::new(builder).unwrap();
    harness.set_log_channel(50);
    harness.invoke(Invocation::new("capture")).await.unwrap();
    let mut ctx = CONTEXT.lock().unwrap().take().unwrap();
    let api = Arc::new(PausedLog {
        inner: Arc::clone(harness.api()),
        entered: Notify::new(),
    });
    ctx.api = ApiClient::new(api.clone(), CapabilitySet::all(), ctx.module.clone())
        .scoped_to(ctx.guild_id);
    let task = tokio::spawn(async move { punish(&ctx, kind).await });
    tokio::time::timeout(Duration::from_secs(5), api.entered.notified())
        .await
        .unwrap();
    assert!(harness.cases().get(1).await.unwrap().is_some());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(harness.jobs(), 1);
}

#[tokio::test]
async fn timeout_timer_survives_aborted_log() {
    timer_survives_aborted_log(Kind::Timeout).await;
}

#[tokio::test]
async fn ban_timer_survives_aborted_log() {
    timer_survives_aborted_log(Kind::Ban).await;
}

#[tokio::test]
async fn voice_timer_survives_aborted_log() {
    timer_survives_aborted_log(Kind::Voice).await;
}

#[tokio::test]
async fn repeated_punishments_replace_the_old_timer() {
    let _guard = TEST_LOCK.lock().await;
    for kind in [Kind::Timeout, Kind::Ban, Kind::Voice] {
        let mut builder = ModuleBuilder::new("race", "Race", "1.0.0").unwrap();
        for capability in Capability::ALL {
            builder = builder.capability(capability);
        }
        let builder = punish::jobs(
            builder
                .config(json!({ "type": "object", "properties": {} }), json!({}))
                .command(Command::<()>::new("capture").handler(capture)),
        );
        let harness = Harness::new(builder).unwrap();
        harness.invoke(Invocation::new("capture")).await.unwrap();
        let ctx = CONTEXT.lock().unwrap().take().unwrap();
        punish(&ctx, kind).await;
        harness.advance(Duration::from_secs(600));
        punish(&ctx, kind).await;
        assert_eq!(harness.jobs(), 1);
        harness.advance(Duration::from_mins(50));
        assert!(harness.run_due_jobs().await.is_empty());
        harness.advance(Duration::from_secs(600));
        let ended = harness.run_due_jobs().await;
        assert_eq!(ended.len(), 1);
        assert!(ended[0].is_ok(), "{ended:?}");
        assert_eq!(harness.jobs(), 0);
    }
}

async fn undo(ctx: &EventContext, kind: Kind, case: i64) {
    let settings = Settings {
        dm: false,
        timeout: Method::Role,
        timeout_role: Some(Id::new(30)),
        voice: Method::Role,
        voice_role: Some(Id::new(30)),
        ..Settings::default()
    };
    let moderator = Moderator::bot(ctx, "test").unwrap();
    let punisher = Punisher::new(ctx, moderator, settings).undoing(case);
    match kind {
        Kind::Timeout => punisher.lift_timeout(Id::new(7), None).await,
        Kind::Ban => punisher.unban(Id::new(7), None).await,
        Kind::Voice => punisher.voice_unmute(Id::new(7), None).await,
    }
    .unwrap();
}

#[tokio::test]
async fn undo_waits_before_a_new_punishment_can_replace_it() {
    let _guard = TEST_LOCK.lock().await;
    for kind in [Kind::Timeout, Kind::Ban, Kind::Voice] {
        let mut builder = ModuleBuilder::new("race", "Race", "1.0.0").unwrap();
        for capability in Capability::ALL {
            builder = builder.capability(capability);
        }
        let builder = punish::jobs(
            builder
                .config(json!({ "type": "object", "properties": {} }), json!({}))
                .command(Command::<()>::new("capture").handler(capture)),
        );
        let harness = Harness::new(builder).unwrap();
        harness.invoke(Invocation::new("capture")).await.unwrap();
        let mut ctx = CONTEXT.lock().unwrap().take().unwrap();
        let api = Arc::new(PausedApi {
            inner: Arc::clone(harness.api()),
            armed: AtomicBool::new(false),
            held: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        });
        ctx.api = ApiClient::new(api.clone(), CapabilitySet::all(), ctx.module.clone())
            .scoped_to(ctx.guild_id);
        let old = punish(&ctx, kind).await;
        api.armed.store(true, Ordering::SeqCst);
        let old_ctx = ctx.clone();
        let old_task = tokio::spawn(async move { undo(&old_ctx, kind, old).await });
        tokio::time::timeout(Duration::from_secs(5), api.entered.notified())
            .await
            .unwrap();
        let new_ctx = ctx.clone();
        let mut new_task = tokio::spawn(async move { punish(&new_ctx, kind).await });
        let early = tokio::time::timeout(Duration::from_millis(100), &mut new_task).await;
        api.release.notify_one();
        old_task.await.unwrap();
        let new_case = match early {
            Ok(result) => result.unwrap(),
            Err(_) => new_task.await.unwrap(),
        };
        assert!(api.held.load(Ordering::SeqCst));
        assert!(
            harness
                .cases()
                .get(new_case)
                .await
                .unwrap()
                .unwrap()
                .revoked
                .is_none()
        );
        assert_eq!(harness.jobs(), 1);
    }
}

struct BanApi {
    inner: Arc<MockApi>,
    banned: AtomicBool,
    pause: AtomicBool,
    pause_unban: AtomicBool,
    fail_list: AtomicBool,
    lookups: AtomicUsize,
    entered: Notify,
}

#[async_trait]
impl DiscordApi for BanApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        match action {
            Action::ListBans { .. } => {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                if self.fail_list.load(Ordering::SeqCst) {
                    return Err(DiscordError::Fatal("offline".into()));
                }
                let bans = if self.banned.load(Ordering::SeqCst) {
                    vec![serde_json::from_value(json!({
                        "reason": null,
                        "user": {"id": "7", "username": "target", "discriminator": "0", "avatar": null}
                    })).unwrap()]
                } else {
                    vec![]
                };
                Ok(Outcome::Bans(bans))
            }
            Action::Ban { .. } => {
                let result = self.inner.execute(action).await;
                if result.is_ok() {
                    self.banned.store(true, Ordering::SeqCst);
                    if self.pause.load(Ordering::SeqCst) {
                        self.entered.notify_one();
                        std::future::pending::<()>().await;
                    }
                }
                result
            }
            Action::Unban { .. } => {
                let result = self.inner.execute(action).await;
                if result.is_ok() {
                    self.banned.store(false, Ordering::SeqCst);
                    if self.pause_unban.load(Ordering::SeqCst) {
                        self.entered.notify_one();
                        std::future::pending::<()>().await;
                    }
                }
                result
            }
            _ => self.inner.execute(action).await,
        }
    }

    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        self.inner.channel_guild(channel).await
    }
}

async fn ban_recovery_fixture() -> (Harness, EventContext, Arc<BanApi>) {
    let mut builder = ModuleBuilder::new("race", "Race", "1.0.0").unwrap();
    for capability in Capability::ALL {
        builder = builder.capability(capability);
    }
    let harness = Harness::new(punish::jobs(
        builder
            .config(json!({ "type": "object", "properties": {} }), json!({}))
            .command(Command::<()>::new("capture").handler(capture)),
    ))
    .unwrap();
    harness.invoke(Invocation::new("capture")).await.unwrap();
    let mut ctx = CONTEXT.lock().unwrap().take().unwrap();
    let api = Arc::new(BanApi {
        inner: Arc::clone(harness.api()),
        banned: AtomicBool::new(false),
        pause: AtomicBool::new(false),
        pause_unban: AtomicBool::new(false),
        fail_list: AtomicBool::new(false),
        lookups: AtomicUsize::new(0),
        entered: Notify::new(),
    });
    ctx.api = ApiClient::new(api.clone(), CapabilitySet::all(), ctx.module.clone())
        .scoped_to(ctx.guild_id);
    (harness, ctx, api)
}

async fn run_ban_recovery(ctx: &EventContext) {
    let module = punish::jobs(ModuleBuilder::new("race", "Race", "1.0.0").unwrap())
        .build()
        .unwrap();
    let job = ScheduledJob {
        id: 1,
        guild_id: ctx.guild_id,
        module: ctx.module.clone(),
        name: "punish_reconcile".into(),
        payload: json!(null),
        attempt: 1,
        token: 1,
    };
    module
        .on_event_flow(ctx, &closure_core::event::Event::Scheduled(job))
        .await
        .unwrap();
}

async fn put_ban_intent(ctx: &EventContext, expires_at: i64) {
    ctx.store()
        .set(
            "ban-intent:7",
            &json!({
                "target": 7, "moderator": ctx.bot().unwrap().get(), "reason": null,
                "started_at": ctx.now(), "expires_at": expires_at, "case_before": 0
            }),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn timed_ban_recovery_after_abort_restores_case_and_timer() {
    let _guard = TEST_LOCK.lock().await;
    let (harness, ctx, api) = ban_recovery_fixture().await;
    api.pause.store(true, Ordering::SeqCst);
    let task_ctx = ctx.clone();
    let task = tokio::spawn(async move { punish(&task_ctx, Kind::Ban).await });
    api.entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(ctx.cases().get(1).await.unwrap().is_none());
    run_ban_recovery(&ctx).await;
    assert_eq!(
        ctx.cases().get(1).await.unwrap().unwrap().kind.as_str(),
        "ban"
    );
    assert_eq!(harness.jobs(), 1);
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_none());
}

#[tokio::test]
async fn a_ban_for_good_that_stopped_before_its_case_is_finished_from_its_intent() {
    let _guard = TEST_LOCK.lock().await;
    let (harness, ctx, api) = ban_recovery_fixture().await;
    api.pause.store(true, Ordering::SeqCst);
    let task_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let settings = Settings {
            dm: false,
            ..Settings::default()
        };
        let moderator = Moderator::bot(&task_ctx, "test").unwrap();
        Punisher::new(&task_ctx, moderator, settings)
            .ban(Id::new(7), None, 0, Some("raid".to_owned()))
            .await
    });
    api.entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(ctx.cases().get(1).await.unwrap().is_none());
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_some());
    run_ban_recovery(&ctx).await;
    let case = ctx.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "ban");
    assert_eq!(case.expires_at, None);
    assert_eq!(case.reason.as_deref(), Some("raid"));
    assert_eq!(harness.jobs(), 0, "a ban for good has no timer");
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_none());
}

#[tokio::test]
async fn an_unban_that_stopped_before_its_case_is_finished_and_the_ban_case_is_revoked() {
    let _guard = TEST_LOCK.lock().await;
    let (_, ctx, api) = ban_recovery_fixture().await;
    api.banned.store(true, Ordering::SeqCst);
    ctx.cases()
        .create(closure_sdk::NewCase {
            source: closure_sdk::CaseSource::Command,
            kind: closure_sdk::CaseKind::new("ban").unwrap(),
            target: Id::new(7),
            moderator: ctx.bot().unwrap(),
            reason: None,
            expires_at: None,
        })
        .await
        .unwrap();
    api.pause_unban.store(true, Ordering::SeqCst);
    let task_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let moderator = Moderator::bot(&task_ctx, "test").unwrap();
        Punisher::new(&task_ctx, moderator, Settings::default())
            .unban(Id::new(7), Some("sorry".to_owned()))
            .await
    });
    api.entered.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(ctx.cases().get(2).await.unwrap().is_none());
    api.pause_unban.store(false, Ordering::SeqCst);
    run_ban_recovery(&ctx).await;
    let unban = ctx.cases().get(2).await.unwrap().unwrap();
    assert_eq!(unban.kind.as_str(), "unban");
    assert!(ctx.cases().get(1).await.unwrap().unwrap().revoked.is_some());
    assert!(ctx.store().entry("unban-intent:7").await.unwrap().is_none());
}

#[tokio::test]
async fn a_softban_that_stopped_after_its_ban_is_lifted_and_recorded_by_the_recovery() {
    let _guard = TEST_LOCK.lock().await;
    let (_, ctx, api) = ban_recovery_fixture().await;
    closure_sdk::punish::remember_softban(&ctx, ctx.bot().unwrap(), Id::new(7), Some("spam"))
        .await
        .unwrap();
    // The ban went out and the process stopped before the unban.
    api.banned.store(true, Ordering::SeqCst);
    run_ban_recovery(&ctx).await;
    assert!(!api.banned.load(Ordering::SeqCst), "the ban was lifted");
    let case = ctx.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "softban");
    assert!(
        ctx.store()
            .entry("softban-intent:7")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_softban_recovery_leaves_a_ban_that_stands_as_a_case_of_its_own() {
    let _guard = TEST_LOCK.lock().await;
    let (_, ctx, api) = ban_recovery_fixture().await;
    closure_sdk::punish::remember_softban(&ctx, ctx.bot().unwrap(), Id::new(7), None)
        .await
        .unwrap();
    api.banned.store(true, Ordering::SeqCst);
    ctx.cases()
        .create(closure_sdk::NewCase {
            source: closure_sdk::CaseSource::Command,
            kind: closure_sdk::CaseKind::new("ban").unwrap(),
            target: Id::new(7),
            moderator: ctx.bot().unwrap(),
            reason: None,
            expires_at: None,
        })
        .await
        .unwrap();
    run_ban_recovery(&ctx).await;
    assert!(
        api.banned.load(Ordering::SeqCst),
        "somebody else's ban is not the softban's to lift"
    );
    assert!(
        ctx.store()
            .entry("softban-intent:7")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(ctx.cases().get(2).await.unwrap(), None);
}

#[tokio::test]
async fn timed_ban_recovery_discards_absent_ban() {
    let _guard = TEST_LOCK.lock().await;
    let (_, ctx, _) = ban_recovery_fixture().await;
    put_ban_intent(&ctx, ctx.now() + 3600).await;
    run_ban_recovery(&ctx).await;
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_none());
    assert!(ctx.cases().get(1).await.unwrap().is_none());
}

#[tokio::test]
async fn timed_ban_recovery_does_not_duplicate_case() {
    let _guard = TEST_LOCK.lock().await;
    let (harness, ctx, api) = ban_recovery_fixture().await;
    let expires = ctx.now() + 3600;
    put_ban_intent(&ctx, expires).await;
    api.banned.store(true, Ordering::SeqCst);
    ctx.cases()
        .create(closure_sdk::NewCase {
            source: closure_sdk::CaseSource::Command,
            kind: closure_sdk::CaseKind::new("ban").unwrap(),
            target: Id::new(7),
            moderator: ctx.bot().unwrap(),
            reason: None,
            expires_at: Some(expires),
        })
        .await
        .unwrap();
    run_ban_recovery(&ctx).await;
    assert!(ctx.cases().get(2).await.unwrap().is_none());
    assert_eq!(harness.jobs(), 1);
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_none());
}

#[tokio::test]
async fn timed_ban_recovery_ignores_a_matching_case_from_before_the_intent() {
    let _guard = TEST_LOCK.lock().await;
    let (_, ctx, api) = ban_recovery_fixture().await;
    let expires = ctx.now() + 3600;
    ctx.cases()
        .create(closure_sdk::NewCase {
            source: closure_sdk::CaseSource::Command,
            kind: closure_sdk::CaseKind::new("ban").unwrap(),
            target: Id::new(7),
            moderator: ctx.bot().unwrap(),
            reason: None,
            expires_at: Some(expires),
        })
        .await
        .unwrap();
    ctx.store()
        .set(
            "ban-intent:7",
            &json!({
                "target": 7, "moderator": ctx.bot().unwrap().get(), "reason": null,
                "started_at": ctx.now(), "expires_at": expires, "case_before": 1
            }),
        )
        .await
        .unwrap();
    api.banned.store(true, Ordering::SeqCst);
    run_ban_recovery(&ctx).await;
    assert!(ctx.cases().get(2).await.unwrap().is_some());
}

#[tokio::test]
async fn timed_ban_recovery_unbans_expired_intent() {
    let _guard = TEST_LOCK.lock().await;
    let (harness, ctx, api) = ban_recovery_fixture().await;
    put_ban_intent(&ctx, ctx.now() - 1).await;
    api.banned.store(true, Ordering::SeqCst);
    run_ban_recovery(&ctx).await;
    assert!(!api.banned.load(Ordering::SeqCst));
    assert!(ctx.cases().get(1).await.unwrap().is_some());
    assert_eq!(harness.jobs(), 0);
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_none());
}

#[tokio::test]
async fn timed_ban_recovery_keeps_intent_when_lookup_fails() {
    let _guard = TEST_LOCK.lock().await;
    let (_, ctx, api) = ban_recovery_fixture().await;
    put_ban_intent(&ctx, ctx.now() + 3600).await;
    api.fail_list.store(true, Ordering::SeqCst);
    run_ban_recovery(&ctx).await;
    assert_eq!(api.lookups.load(Ordering::SeqCst), 1);
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_some());
    assert!(ctx.cases().get(1).await.unwrap().is_none());
}

#[tokio::test]
async fn timed_ban_recovery_keeps_intent_without_a_case_boundary() {
    let _guard = TEST_LOCK.lock().await;
    let (_, ctx, api) = ban_recovery_fixture().await;
    ctx.store()
        .set(
            "ban-intent:7",
            &json!({
                "target": 7, "moderator": ctx.bot().unwrap().get(), "reason": null,
                "started_at": ctx.now(), "expires_at": ctx.now() - 1, "case_before": null
            }),
        )
        .await
        .unwrap();
    api.banned.store(true, Ordering::SeqCst);
    run_ban_recovery(&ctx).await;
    assert!(api.banned.load(Ordering::SeqCst));
    assert!(ctx.cases().get(1).await.unwrap().is_none());
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_some());
}

#[tokio::test]
async fn timed_ban_recovery_moves_past_a_batch_of_failures() {
    let _guard = TEST_LOCK.lock().await;
    let (_, ctx, api) = ban_recovery_fixture().await;
    for user in 100..=150 {
        ctx.store()
            .set(
                &format!("ban-intent:{user}"),
                &json!({
                    "target": user, "moderator": ctx.bot().unwrap().get(), "reason": null,
                    "started_at": ctx.now(), "expires_at": ctx.now() + 3600, "case_before": 0
                }),
            )
            .await
            .unwrap();
    }
    api.fail_list.store(true, Ordering::SeqCst);
    run_ban_recovery(&ctx).await;
    assert_eq!(api.lookups.load(Ordering::SeqCst), 50);
    api.fail_list.store(false, Ordering::SeqCst);
    run_ban_recovery(&ctx).await;
    assert!(ctx.store().entry("ban-intent:150").await.unwrap().is_none());
    assert!(ctx.store().entry("ban-intent:100").await.unwrap().is_some());
}

#[tokio::test]
async fn timed_ban_recovery_runs_on_startup_and_every_five_minutes() {
    let _guard = TEST_LOCK.lock().await;
    let (harness, ctx, _) = ban_recovery_fixture().await;
    put_ban_intent(&ctx, ctx.now() + 3600).await;
    harness
        .emit(closure_core::event::Event::GuildAvailable {
            guild_id: Id::new(1),
            name: "test".into(),
            preferred_locale: None,
        })
        .await
        .unwrap();
    assert!(harness.run_due_jobs().await.iter().all(Result::is_ok));
    harness.advance(Duration::from_secs(300));
    assert!(harness.run_due_jobs().await.iter().all(Result::is_ok));
    assert_eq!(
        harness
            .api()
            .actions()
            .iter()
            .filter(|action| matches!(action, Action::ListBans { .. }))
            .count(),
        2
    );
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_some());
}

#[tokio::test]
async fn timed_ban_recovery_precedes_an_older_unban_timer() {
    let _guard = TEST_LOCK.lock().await;
    let (harness, ctx, api) = ban_recovery_fixture().await;
    put_ban_intent(&ctx, ctx.now() + 3600).await;
    api.banned.store(true, Ordering::SeqCst);
    let module = punish::jobs(ModuleBuilder::new("race", "Race", "1.0.0").unwrap())
        .build()
        .unwrap();
    let job = ScheduledJob {
        id: 1,
        guild_id: ctx.guild_id,
        module: ctx.module.clone(),
        name: "unban".into(),
        payload: json!({ "user": 7, "case": 0 }),
        attempt: 1,
        token: 1,
    };
    module
        .on_event_flow(&ctx, &closure_core::event::Event::Scheduled(job))
        .await
        .unwrap();
    assert!(api.banned.load(Ordering::SeqCst));
    assert_eq!(harness.jobs(), 1);
    assert!(ctx.store().entry("ban-intent:7").await.unwrap().is_none());
}

#[derive(Clone, Copy)]
enum RestrictionKind {
    RoleTimeout,
    DiscordTimeout,
    RoleVoice,
    DiscordVoice,
}

impl RestrictionKind {
    fn kind(self) -> &'static str {
        match self {
            Self::RoleTimeout | Self::DiscordTimeout => "timeout",
            _ => "vcmute",
        }
    }
    fn role(self) -> bool {
        matches!(self, Self::RoleTimeout | Self::RoleVoice)
    }
}

struct RestrictionApi {
    inner: Arc<MockApi>,
    mode: RestrictionKind,
    present: AtomicBool,
    pause: AtomicBool,
    failure: AtomicUsize,
    entered: Notify,
}

#[async_trait]
impl DiscordApi for RestrictionApi {
    async fn execute(&self, action: Action) -> Result<Outcome, DiscordError> {
        if matches!(&action, Action::AddRole { role_id, .. } if role_id.get() == 99)
            && self.failure.load(Ordering::SeqCst) == 3
        {
            return Err(DiscordError::Fatal("offline".into()));
        }
        if matches!(action, Action::GetMember { .. }) {
            match self.failure.load(Ordering::SeqCst) {
                1 => return Err(DiscordError::Fatal("offline".into())),
                2 => return Err(DiscordError::NotFound),
                _ => {}
            }
            let present = self.present.load(Ordering::SeqCst);
            let member = serde_json::from_value(json!({
                "deaf": false, "mute": present && matches!(self.mode, RestrictionKind::DiscordVoice),
                "flags": 0, "roles": if present && self.mode.role() { vec!["30"] } else { vec![] },
                "communication_disabled_until": if present && matches!(self.mode, RestrictionKind::DiscordTimeout) { Some("2099-01-01T00:00:00.000000+00:00") } else { None },
                "user": {"id":"7", "username":"target", "discriminator":"0", "avatar":null}
            })).unwrap();
            return Ok(Outcome::Member(Box::new(member)));
        }
        let give = matches!(&action, Action::AddRole { role_id, .. } if role_id.get() == 30)
            || matches!(
                action,
                Action::Timeout {
                    duration_seconds: Some(_),
                    ..
                } | Action::SetVoiceMute { mute: true, .. }
            );
        let lift = matches!(&action, Action::RemoveRole { role_id, .. } if role_id.get() == 30)
            || matches!(
                action,
                Action::Timeout {
                    duration_seconds: None,
                    ..
                } | Action::SetVoiceMute { mute: false, .. }
            );
        let result = self.inner.execute(action).await;
        if result.is_ok() && (give || lift) {
            self.present.store(give, Ordering::SeqCst);
            if give && self.pause.load(Ordering::SeqCst) {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
        }
        result
    }

    async fn channel_guild(&self, channel: ChannelId) -> Result<Option<GuildId>, DiscordError> {
        self.inner.channel_guild(channel).await
    }
}

async fn restriction_fixture(
    mode: RestrictionKind,
) -> (Harness, EventContext, Arc<RestrictionApi>) {
    let (harness, mut ctx, _) = ban_recovery_fixture().await;
    let api = Arc::new(RestrictionApi {
        inner: Arc::clone(harness.api()),
        mode,
        present: AtomicBool::new(false),
        pause: AtomicBool::new(false),
        failure: AtomicUsize::new(0),
        entered: Notify::new(),
    });
    ctx.api = ApiClient::new(api.clone(), CapabilitySet::all(), ctx.module.clone())
        .scoped_to(ctx.guild_id);
    (harness, ctx, api)
}

async fn put_restriction_intent(ctx: &EventContext, mode: RestrictionKind, expires_at: i64) {
    ctx.store().set(&format!("{}-intent:7", mode.kind()), &json!({
        "kind": mode.kind(), "target": 7, "moderator": ctx.bot().unwrap().get(), "reason": null,
        "started_at": ctx.now(), "expires_at": expires_at, "case_before": 0,
        "method": if mode.role() { "role" } else { "discord" },
        "role": if mode.role() { Some(30) } else { None }, "restore": "All"
    })).await.unwrap();
}

const RESTRICTIONS: [RestrictionKind; 4] = [
    RestrictionKind::RoleTimeout,
    RestrictionKind::DiscordTimeout,
    RestrictionKind::RoleVoice,
    RestrictionKind::DiscordVoice,
];

#[tokio::test]
async fn restriction_recovery_after_abort_restores_case_and_timer() {
    let _guard = TEST_LOCK.lock().await;
    for mode in RESTRICTIONS {
        let (harness, ctx, api) = restriction_fixture(mode).await;
        api.pause.store(true, Ordering::SeqCst);
        let task_ctx = ctx.clone();
        let task = tokio::spawn(async move {
            let settings = Settings {
                dm: false,
                timeout: if mode.role() {
                    Method::Role
                } else {
                    Method::Discord
                },
                voice: if mode.role() {
                    Method::Role
                } else {
                    Method::Discord
                },
                timeout_role: Some(Id::new(30)),
                voice_role: Some(Id::new(30)),
                ..Settings::default()
            };
            let punisher = Punisher::new(
                &task_ctx,
                Moderator::bot(&task_ctx, "test").unwrap(),
                settings,
            );
            if mode.kind() == "timeout" {
                punisher
                    .timeout(Id::new(7), Duration::from_secs(3600), None)
                    .await
                    .unwrap();
            } else {
                punisher
                    .voice_mute(Id::new(7), Some(Duration::from_secs(3600)), None)
                    .await
                    .unwrap();
            }
        });
        tokio::time::timeout(Duration::from_secs(5), api.entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(ctx.cases().get(1).await.unwrap().is_none());
        run_ban_recovery(&ctx).await;
        assert_eq!(
            ctx.cases().get(1).await.unwrap().unwrap().kind.as_str(),
            mode.kind()
        );
        assert_eq!(
            harness.jobs(),
            usize::from(!matches!(mode, RestrictionKind::DiscordTimeout))
        );
        assert!(
            ctx.store()
                .entry(&format!("{}-intent:7", mode.kind()))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn restriction_recovery_handles_absent_duplicate_expired_and_lookup_errors() {
    let _guard = TEST_LOCK.lock().await;
    for mode in RESTRICTIONS {
        let (harness, ctx, api) = restriction_fixture(mode).await;
        let key = format!("{}-intent:7", mode.kind());
        put_restriction_intent(&ctx, mode, ctx.now() + 3600).await;
        api.failure.store(1, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        assert!(ctx.store().entry(&key).await.unwrap().is_some());
        api.failure.store(2, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        assert!(ctx.store().entry(&key).await.unwrap().is_none());
        api.failure.store(0, Ordering::SeqCst);
        put_restriction_intent(&ctx, mode, ctx.now() + 3600).await;
        run_ban_recovery(&ctx).await;
        assert!(ctx.store().entry(&key).await.unwrap().is_none());
        assert!(ctx.cases().get(1).await.unwrap().is_none());
        let expires = ctx.now() + 3600;
        put_restriction_intent(&ctx, mode, expires).await;
        api.present.store(true, Ordering::SeqCst);
        ctx.cases()
            .create(closure_sdk::NewCase {
                source: closure_sdk::CaseSource::Command,
                kind: closure_sdk::CaseKind::new(mode.kind()).unwrap(),
                target: Id::new(7),
                moderator: Id::new(8),
                reason: Some("another reason".into()),
                expires_at: Some(expires + 1),
            })
            .await
            .unwrap();
        run_ban_recovery(&ctx).await;
        assert!(ctx.cases().get(2).await.unwrap().is_none());
        assert_eq!(
            harness.jobs(),
            usize::from(!matches!(mode, RestrictionKind::DiscordTimeout))
        );
        let (harness, ctx, api) = restriction_fixture(mode).await;
        put_restriction_intent(&ctx, mode, ctx.now() - 1).await;
        api.present.store(true, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        assert!(!api.present.load(Ordering::SeqCst));
        assert!(ctx.cases().get(1).await.unwrap().is_some());
        assert_eq!(harness.jobs(), 0);
        assert!(ctx.store().entry(&key).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn restriction_recovery_moves_past_a_batch_of_lookup_failures() {
    let _guard = TEST_LOCK.lock().await;
    for mode in [RestrictionKind::RoleTimeout, RestrictionKind::RoleVoice] {
        let (_, ctx, api) = restriction_fixture(mode).await;
        for user in 100..=150 {
            ctx.store().set(&format!("{}-intent:{user}", mode.kind()), &json!({
                "kind": mode.kind(), "target": user, "moderator": ctx.bot().unwrap().get(), "reason": null,
                "started_at": ctx.now(), "expires_at": ctx.now() + 3600, "case_before": 0,
                "method": "role", "role": 30, "restore": "All"
            })).await.unwrap();
        }
        api.failure.store(1, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        api.failure.store(2, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        assert!(
            ctx.store()
                .entry(&format!("{}-intent:150", mode.kind()))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            ctx.store()
                .entry(&format!("{}-intent:100", mode.kind()))
                .await
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn restriction_recovery_ignores_a_case_from_before_the_intent() {
    let _guard = TEST_LOCK.lock().await;
    for mode in RESTRICTIONS {
        let (_, ctx, api) = restriction_fixture(mode).await;
        let expires = ctx.now() + 3600;
        ctx.cases()
            .create(closure_sdk::NewCase {
                source: closure_sdk::CaseSource::Command,
                kind: closure_sdk::CaseKind::new(mode.kind()).unwrap(),
                target: Id::new(7),
                moderator: ctx.bot().unwrap(),
                reason: None,
                expires_at: Some(expires),
            })
            .await
            .unwrap();
        put_restriction_intent(&ctx, mode, expires).await;
        let key = format!("{}-intent:7", mode.kind());
        let mut intent: serde_json::Value = ctx.store().get(&key).await.unwrap().unwrap();
        intent["case_before"] = json!(1);
        ctx.store().set(&key, &intent).await.unwrap();
        api.present.store(true, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        assert!(ctx.cases().get(2).await.unwrap().is_some());
    }
}

#[tokio::test]
async fn restriction_recovery_restores_stripped_roles_on_expiry() {
    let _guard = TEST_LOCK.lock().await;
    for mode in [RestrictionKind::RoleTimeout, RestrictionKind::RoleVoice] {
        let (harness, ctx, api) = restriction_fixture(mode).await;
        ctx.restrictions()
            .set("stripped:7", &vec![99_u64])
            .await
            .unwrap();
        put_restriction_intent(&ctx, mode, ctx.now() - 1).await;
        api.present.store(true, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        assert!(harness.api().actions().iter().any(
            |action| matches!(action, Action::AddRole { role_id, .. } if role_id.get() == 99)
        ));
        assert!(
            ctx.restrictions()
                .get::<Vec<u64>>("stripped:7")
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn restriction_recovery_retries_restore_after_role_was_removed() {
    let _guard = TEST_LOCK.lock().await;
    for mode in [RestrictionKind::RoleTimeout, RestrictionKind::RoleVoice] {
        let (harness, ctx, api) = restriction_fixture(mode).await;
        ctx.restrictions()
            .set("stripped:7", &vec![99_u64])
            .await
            .unwrap();
        put_restriction_intent(&ctx, mode, ctx.now() - 1).await;
        api.present.store(true, Ordering::SeqCst);
        api.failure.store(3, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        assert!(!api.present.load(Ordering::SeqCst));
        assert!(
            ctx.store()
                .entry(&format!("{}-intent:7", mode.kind()))
                .await
                .unwrap()
                .is_some()
        );
        api.failure.store(0, Ordering::SeqCst);
        run_ban_recovery(&ctx).await;
        assert!(harness.api().actions().iter().any(
            |action| matches!(action, Action::AddRole { role_id, .. } if role_id.get() == 99)
        ));
        assert!(
            ctx.restrictions()
                .get::<Vec<u64>>("stripped:7")
                .await
                .unwrap()
                .is_none()
        );
        assert!(ctx.cases().get(2).await.unwrap().is_none());
        assert!(
            ctx.store()
                .entry(&format!("{}-intent:7", mode.kind()))
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn restriction_recovery_precedes_an_older_lift_timer() {
    let _guard = TEST_LOCK.lock().await;
    for mode in [
        RestrictionKind::RoleTimeout,
        RestrictionKind::RoleVoice,
        RestrictionKind::DiscordVoice,
    ] {
        let (harness, ctx, api) = restriction_fixture(mode).await;
        put_restriction_intent(&ctx, mode, ctx.now() + 3600).await;
        api.present.store(true, Ordering::SeqCst);
        let (name, payload) = if mode.kind() == "timeout" {
            ("untimeout", json!({"user": 7, "case": 0, "role": 30}))
        } else {
            (
                "vc_unmute",
                json!({"user": 7, "case": 0, "via": if mode.role() { "role" } else { "discord" }, "role": if mode.role() { Some(30) } else { None }, "due": ctx.now()}),
            )
        };
        let module = punish::jobs(ModuleBuilder::new("race", "Race", "1.0.0").unwrap())
            .build()
            .unwrap();
        let job = ScheduledJob {
            id: 1,
            guild_id: ctx.guild_id,
            module: ctx.module.clone(),
            name: name.into(),
            payload,
            attempt: 1,
            token: 1,
        };
        module
            .on_event_flow(&ctx, &closure_core::event::Event::Scheduled(job))
            .await
            .unwrap();
        assert!(api.present.load(Ordering::SeqCst));
        assert_eq!(harness.jobs(), 1);
        assert!(
            ctx.store()
                .entry(&format!("{}-intent:7", mode.kind()))
                .await
                .unwrap()
                .is_none()
        );
    }
}
