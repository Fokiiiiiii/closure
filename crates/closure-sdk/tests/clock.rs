//! The clock of a module: what `now` says in a command and in a job, and how a test moves it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use closure_core::error::ModuleError;
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Capability, Command, ModuleBuilder};

fn system_now() -> i64 {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(elapsed.as_secs()).unwrap()
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("clock", "Clock", "0.1.0")
        .unwrap()
        .capability(Capability::Storage)
        .capability(Capability::Schedule)
        .command(
            Command::<()>::new("now")
                .handler(|cx, ()| async move { cx.reply(cx.now().to_string()).await }),
        )
        .command(Command::<()>::new("later").handler(|cx, ()| async move {
            cx.scheduler()
                .job("stamp", &())?
                .after(Duration::from_secs(10))
                .await?;
            Ok(())
        }))
        .job("stamp", |cx| async move {
            cx.store()
                .set("stamp", &cx.now())
                .await
                .map_err(|error| ModuleError::other(error.to_string()))
        })
}

async fn time_of(harness: &Harness, command: &str) -> i64 {
    let replies = harness.invoke(Invocation::new(command)).await.unwrap();
    replies[0].content.parse().unwrap()
}

#[tokio::test]
async fn a_command_and_a_job_see_the_clock_of_the_harness() {
    let harness = Harness::new(module()).unwrap();

    let first = time_of(&harness, "now").await;
    assert!(
        (first - system_now()).abs() < 60,
        "the clock starts at the time of the test: {first}"
    );

    harness.advance(Duration::from_secs(90));
    let second = time_of(&harness, "now").await;
    assert_eq!(second - first, 90, "advance moves the clock");

    harness.invoke(Invocation::new("later")).await.unwrap();
    harness.advance(Duration::from_secs(10));
    let ended = harness.run_due_jobs().await;
    assert!(ended.iter().all(Result::is_ok), "{ended:?}");
    let stamped: i64 = harness.store().get("stamp").await.unwrap().unwrap();
    assert_eq!(stamped - second, 10, "the job runs ten seconds later");
}
