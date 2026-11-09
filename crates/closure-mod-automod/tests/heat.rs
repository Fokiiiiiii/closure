//! The heat example in `docs/examples/heat.yaml`: it is a valid configuration, and a raid that
//! feeds the counter raises the antiraid level, which falls back by itself.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a broken setup should panic at once"
)]

mod common;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use closure_config::{ConfigCompiler, parse_overrides};
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use closure_sdk::CommandOptionValue;
use closure_sdk::Event;
use closure_sdk::testing::{Harness, Invocation};
use common::{GUILD, configured, unix_now};
use serde_json::Value;
use twilight_model::id::Id;

const EXAMPLE: &str = include_str!("../../../docs/examples/heat.yaml");
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// The example as the `config` of the automod module in a server's overrides.
fn overrides() -> String {
    let indented = EXAMPLE
        .lines()
        .map(|line| format!("      {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("modules:\n  automod:\n    config:\n{indented}\n")
}

fn config() -> Value {
    parse_overrides(&overrides())
        .unwrap()
        .modules
        .get("automod")
        .and_then(|module| module.config.clone())
        .expect("the example has a config")
}

/// A member whose account is `old` (made long ago) or made just now.
fn joining(index: u64, old: bool) -> Event {
    let id = if old {
        7_000 + index
    } else {
        let now_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        ((now_ms - DISCORD_EPOCH_MS) << 22) | (index + 1)
    };
    Event::MemberJoined(closure_core::event::MemberInfo {
        guild_id: Id::new(GUILD),
        user: closure_core::event::UserInfo {
            id: Id::new(id),
            name: "member".into(),
            bot: false,
        },
        roles: Vec::new(),
        nick: None,
        joined_at: Some(unix_now()),
        timed_out_until: None,
    })
}

async fn level(harness: &Harness) -> String {
    harness
        .store()
        .get::<Value>("antiraid:level")
        .await
        .unwrap()
        .map(|value| value.to_string())
        .unwrap_or_default()
}

#[test]
fn the_example_is_a_valid_configuration() {
    let registry = Arc::new(
        ModuleRegistry::new(vec![
            Arc::new(closure_mod_automod::module().unwrap()) as Arc<dyn Module>
        ])
        .unwrap(),
    );
    let compiler = ConfigCompiler::new(registry).unwrap();
    compiler
        .compile(&parse_overrides(&overrides()).unwrap())
        .expect("the example compiles");
}

#[tokio::test]
async fn heat_raises_the_level_step_by_step_and_it_falls_back() {
    let harness = configured(config());

    for index in 0..9 {
        harness.emit(joining(index, true)).await.unwrap();
    }
    assert!(!level(&harness).await.contains("low"), "18 is below 20");
    harness.emit(joining(9, true)).await.unwrap();
    assert!(level(&harness).await.contains("low"), "20 is warm");

    for index in 10..30 {
        harness.emit(joining(index, false)).await.unwrap();
    }
    assert!(
        level(&harness).await.contains("high"),
        "new accounts make it hot"
    );

    harness.advance(Duration::from_mins(31));
    let ran = harness.run_due_jobs().await;
    assert!(ran.iter().all(Result::is_ok), "{ran:?}");
    assert!(
        !level(&harness).await.contains("high"),
        "the calm level comes back"
    );
}

/// The value of the heat counter, as `/automod counter` shows it.
async fn heat(harness: &Harness) -> i64 {
    let replies = harness
        .invoke(
            Invocation::new("automod")
                .subcommand("counter")
                .option("counter", CommandOptionValue::String("heat".to_owned()))
                .option("action", CommandOptionValue::String("view".to_owned()))
                .roles([common::MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    let text: String = replies.iter().map(|reply| reply.content.clone()).collect();
    let (_, number) = text.rsplit_once(": ").expect("the view shows a value");
    number.trim().parse().expect("the view shows a number")
}

#[tokio::test]
async fn a_new_account_adds_more_heat_than_an_old_one() {
    let harness = configured(config());
    harness.emit(joining(0, true)).await.unwrap();
    assert_eq!(heat(&harness).await, 2, "an old account: the join");
    harness.emit(joining(1, false)).await.unwrap();
    assert_eq!(
        heat(&harness).await,
        2 + 5,
        "a new account: the join and the bonus for its age"
    );
}

#[tokio::test]
async fn adding_heat_keeps_no_case_for_each_join() {
    let harness = configured(config());
    for index in 0..3 {
        harness.emit(joining(index, true)).await.unwrap();
    }
    assert!(
        harness.cases().get(1).await.unwrap().is_none(),
        "a join that only warms the server is no case"
    );
}
