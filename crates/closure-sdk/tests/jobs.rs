//! Work scheduled for later, written with the SDK only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::time::Duration;

use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{
    Capability, Command, CoreError, Event, EventKind, ModuleBuilder, ModuleError, ScheduleError,
    Subscription,
};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Unmute {
    user: u64,
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("mute", "Mutes", "0.1.0")
        .unwrap()
        .capability(Capability::Storage)
        .command(Command::<()>::new("mute").handler(|cx, ()| async move {
            let user = cx.user().id.get();
            cx.ctx()
                .scheduler()
                .job("unmute", &Unmute { user })?
                .key(&format!("unmute:{user}"))
                .after(Duration::from_secs(600))
                .await?;
            cx.reply("Muted.").await
        }))
        .command(Command::<()>::new("unmute").handler(|cx, ()| async move {
            let user = cx.user().id.get();
            let found = cx
                .ctx()
                .scheduler()
                .cancel(&format!("unmute:{user}"))
                .await?;
            cx.reply(if found { "Cancelled." } else { "No timer." })
                .await
        }))
        .command(Command::<()>::new("watch").handler(|cx, ()| async move {
            cx.ctx()
                .scheduler()
                .job("tick", &())?
                .key("tick")
                .every(Duration::from_secs(60), Duration::from_secs(60))
                .await?;
            cx.reply("Watching.").await
        }))
        .command(Command::<()>::new("flaky").handler(|cx, ()| async move {
            cx.ctx()
                .scheduler()
                .job("flaky", &())?
                .after(Duration::ZERO)
                .await?;
            cx.reply("Queued.").await
        }))
        .job("unmute", |cx| async move {
            let who: Unmute = cx.payload()?;
            cx.store()
                .counter_add("unmuted", who.user, 1)
                .await
                .map_err(|error| ModuleError::other(error.to_string()))?;
            Ok(())
        })
        .job("tick", |cx| async move {
            cx.store()
                .counter_add("ticks", 0_u64, 1)
                .await
                .map_err(|error| ModuleError::other(error.to_string()))?;
            Ok(())
        })
        .job("flaky", |cx| async move {
            cx.store()
                .counter_add("flaky-tries", 0_u64, 1)
                .await
                .map_err(|error| ModuleError::other(error.to_string()))?;
            if cx.attempt() == 1 {
                return Err(ModuleError::other("not yet"));
            }
            Ok(())
        })
}

async fn count(harness: &Harness, name: &str, subject: u64) -> i64 {
    harness.store().counter_total(name, subject).await.unwrap()
}

#[tokio::test]
async fn a_command_sets_a_job_that_runs_when_its_time_has_come() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness
        .invoke(Invocation::new("mute").user(7))
        .await
        .unwrap();
    assert_eq!(replies[0].content, "Muted.");
    assert_eq!(harness.jobs(), 1);

    assert!(harness.run_due_jobs().await.is_empty());
    harness.advance(Duration::from_secs(599));
    assert!(harness.run_due_jobs().await.is_empty());
    assert_eq!(count(&harness, "unmuted", 7).await, 0);

    harness.advance(Duration::from_secs(1));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1);
    assert!(ended[0].is_ok());
    assert_eq!(
        count(&harness, "unmuted", 7).await,
        1,
        "the payload reached the handler"
    );
    assert_eq!(harness.jobs(), 0, "a job that ran once is gone");
    assert!(harness.run_due_jobs().await.is_empty());
}

#[tokio::test]
async fn the_same_key_replaces_the_job_and_cancelling_removes_it() {
    let harness = Harness::new(module()).unwrap();
    harness
        .invoke(Invocation::new("mute").user(7))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(300));
    harness
        .invoke(Invocation::new("mute").user(7))
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 1);
    // The first timer would have run by now; the replacement has 300 seconds to go.
    harness.advance(Duration::from_secs(300));
    assert!(harness.run_due_jobs().await.is_empty());

    harness
        .invoke(Invocation::new("mute").user(8))
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 2);
    let replies = harness
        .invoke(Invocation::new("unmute").user(8))
        .await
        .unwrap();
    assert_eq!(replies[0].content, "Cancelled.");
    let replies = harness
        .invoke(Invocation::new("unmute").user(8))
        .await
        .unwrap();
    assert_eq!(replies[0].content, "No timer.");
    assert_eq!(harness.jobs(), 1);
}

#[tokio::test]
async fn a_failed_job_comes_again_after_a_pause_with_its_try_counted() {
    let harness = Harness::new(module()).unwrap();
    harness.invoke(Invocation::new("flaky")).await.unwrap();

    let first = harness.run_due_jobs().await;
    assert_eq!(first.len(), 1);
    assert!(first[0].is_err());
    assert!(harness.run_due_jobs().await.is_empty(), "it waits first");

    harness.advance(Duration::from_secs(5));
    let second = harness.run_due_jobs().await;
    assert!(second[0].is_ok());
    assert_eq!(count(&harness, "flaky-tries", 0).await, 2);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_repeating_job_runs_again_and_again() {
    let harness = Harness::new(module()).unwrap();
    harness.invoke(Invocation::new("watch")).await.unwrap();
    for round in 1..=3 {
        assert!(harness.run_due_jobs().await.is_empty());
        harness.advance(Duration::from_secs(60));
        assert_eq!(harness.run_due_jobs().await.len(), 1);
        assert_eq!(count(&harness, "ticks", 0).await, round);
    }
    assert_eq!(harness.jobs(), 1, "it stays until it is cancelled");
}

#[tokio::test]
async fn a_job_nobody_handles_any_more_is_dropped_and_the_payload_must_fit() {
    let harness = Harness::new(module()).unwrap();
    let scheduler = harness.scheduler();
    scheduler
        .job("removed-in-this-version", &1)
        .unwrap()
        .after(Duration::ZERO)
        .await
        .unwrap();
    scheduler
        .job("unmute", &"not a user")
        .unwrap()
        .after(Duration::ZERO)
        .await
        .unwrap();

    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 2);
    assert!(ended[0].is_ok(), "an old job is no reason to fail");
    let error = ended[1].as_ref().unwrap_err().to_string();
    assert!(error.contains("unmute"), "{error}");
}

#[tokio::test]
async fn a_repeating_job_nobody_handles_any_more_stops() {
    let harness = Harness::new(module()).unwrap();
    harness
        .scheduler()
        .job("removed-in-this-version", &())
        .unwrap()
        .every(Duration::from_secs(60), Duration::ZERO)
        .await
        .unwrap();
    let ended = harness.run_due_jobs().await;
    assert!(ended[0].is_ok());
    assert_eq!(harness.jobs(), 0, "it is not set for its next time");
}

#[tokio::test]
async fn a_module_without_a_job_handler_cannot_schedule() {
    let harness = Harness::new(ModuleBuilder::new("plain", "Plain", "0.1.0").unwrap()).unwrap();
    assert!(matches!(
        harness.scheduler().job("x", &1),
        Err(ScheduleError::NotGranted)
    ));
    // One with a handler can, without asking for the capability.
    let harness = Harness::new(module()).unwrap();
    assert!(harness.scheduler().job("x", &1).is_ok());
}

#[test]
fn jobs_are_declared_with_job_not_subscribed_to_and_only_once() {
    let subscribed = ModuleBuilder::new("a", "A", "0.1.0").unwrap().on(
        Subscription::new(EventKind::Scheduled),
        |_, _: Event| async { Ok(closure_sdk::Flow::Continue) },
    );
    assert!(matches!(
        subscribed.build(),
        Err(CoreError::InvalidCommand { .. })
    ));

    let twice = ModuleBuilder::new("b", "B", "0.1.0")
        .unwrap()
        .job("x", |_| async { Ok(()) })
        .job("x", |_| async { Ok(()) });
    assert!(matches!(
        twice.build(),
        Err(CoreError::InvalidCommand { .. })
    ));
}
