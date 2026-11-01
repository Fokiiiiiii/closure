#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use closure_core::action::{Action, Outcome};
use closure_core::capability::CapabilitySet;
use closure_core::client::{ApiClient, DiscordApi};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, GuildId};
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
