//! Counters: what the actions change, the scopes they keep, decay, the triggers that fire on a
//! change, the chain from a trigger to a rule to a counter, the depth limit, and the commands
//! that look at and change them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a broken setup should panic at once"
)]
#![allow(clippy::too_many_lines, reason = "a scenario reads best as one block")]
#![allow(
    clippy::needless_pass_by_value,
    reason = "helpers take the JSON they embed by value, which keeps the rules readable at the call site"
)]

mod common;

use std::time::Duration;

use closure_core::error::DiscordError;
use closure_core::event::CommandOptionValue;
use closure_sdk::testing::{Harness, Invocation, Message, Reply};
use common::*;
use serde_json::{Value, json};
use twilight_model::id::Id;

/// A harness with these counters and rules.
fn with(counters: Value, rules_: Value) -> Harness {
    configured(json!({ "counters": counters, "rules": rules_ }))
}

fn words(word: &str) -> Value {
    json!({ "type": "words", "words": [word] })
}

fn alert(text: &str) -> Value {
    json!({ "do": "alert", "channel": NOTIFY.to_string(), "text": text })
}

fn scoped_to(mut invocation: Invocation, user: Option<u64>, channel: Option<u64>) -> Invocation {
    if let Some(user) = user {
        invocation = invocation.option("user", CommandOptionValue::User(Id::new(user)));
    }
    if let Some(channel) = channel {
        invocation = invocation.option("channel", CommandOptionValue::Channel(Id::new(channel)));
    }
    invocation
}

/// `/automod counter <counter> action:<sub>` run by a moderator.
fn command(sub: &str, counter: &str) -> Invocation {
    Invocation::new("automod")
        .subcommand("counter")
        .option("counter", CommandOptionValue::String(counter.to_owned()))
        .option("action", CommandOptionValue::String(sub.to_owned()))
        .roles([MODERATOR_ROLE])
}

/// The text of the replies.
fn text_of(replies: &[Reply]) -> String {
    replies
        .iter()
        .map(|reply| reply.content.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The value a `view` shows for the scope, read from the reply (its text ends with `: <value>`).
async fn value(harness: &Harness, counter: &str, user: Option<u64>, channel: Option<u64>) -> i64 {
    let replies = harness
        .invoke(scoped_to(command("view", counter), user, channel))
        .await
        .unwrap();
    let text = text_of(&replies);
    let (_, number) = text
        .rsplit_once(": ")
        .unwrap_or_else(|| panic!("the view shows a value: {text:?}"));
    number
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("the view shows a number: {text:?}"))
}

// ---- what the actions change ----

#[tokio::test]
async fn add_to_counter_keeps_a_count_for_each_member() {
    let harness = with(
        json!({ "strikes": { "per_user": true } }),
        rules(&[(
            "plus",
            rule(
                json!([words("+")]),
                json!([{ "do": "add_to_counter", "counter": "strikes", "amount": 1 }]),
            ),
        )]),
    );
    say(&harness, MEMBER, "+").await;
    say(&harness, MEMBER, "+").await;
    say(&harness, OTHER_MEMBER, "+").await;
    assert_eq!(value(&harness, "strikes", Some(MEMBER), None).await, 2);
    assert_eq!(
        value(&harness, "strikes", Some(OTHER_MEMBER), None).await,
        1
    );
}

#[tokio::test]
async fn set_and_add_need_a_value_and_an_unknown_action_is_refused() {
    let harness = with(json!({ "strikes": {} }), rules(&[]));
    let replies = harness.invoke(command("set", "strikes")).await.unwrap();
    assert!(
        text_of(&replies).contains("Give a value"),
        "{}",
        text_of(&replies)
    );
    let replies = harness.invoke(command("bump", "strikes")).await.unwrap();
    assert!(
        text_of(&replies).contains("Choose view, set, add or reset"),
        "{}",
        text_of(&replies)
    );
}

#[tokio::test]
async fn set_counter_sets_the_value_of_the_member() {
    let harness = with(
        json!({ "strikes": { "per_user": true } }),
        rules(&[(
            "five",
            rule(
                json!([words("five")]),
                json!([{ "do": "set_counter", "counter": "strikes", "value": 5 }]),
            ),
        )]),
    );
    say(&harness, MEMBER, "five").await;
    assert_eq!(value(&harness, "strikes", Some(MEMBER), None).await, 5);
}

#[tokio::test]
async fn a_per_channel_counter_keeps_each_channel_apart() {
    let harness = with(
        json!({ "heat": { "per_channel": true } }),
        rules(&[(
            "plus",
            rule(
                json!([words("+")]),
                json!([{ "do": "add_to_counter", "counter": "heat", "amount": 1 }]),
            ),
        )]),
    );
    say_in(&harness, MEMBER, CHANNEL, "+").await;
    say_in(&harness, MEMBER, CHANNEL, "+").await;
    say_in(&harness, MEMBER, OTHER_CHANNEL, "+").await;
    assert_eq!(value(&harness, "heat", None, Some(CHANNEL)).await, 2);
    assert_eq!(value(&harness, "heat", None, Some(OTHER_CHANNEL)).await, 1);
}

#[tokio::test]
async fn a_counter_change_on_a_join_counts_the_joining_member() {
    let harness = with(
        json!({ "joins": { "per_user": true } }),
        rules(&[(
            "joined",
            rule(
                json!([{ "type": "member_join" }]),
                json!([{ "do": "add_to_counter", "counter": "joins", "amount": 1 }]),
            ),
        )]),
    );
    join(&harness, MEMBER).await;
    assert_eq!(value(&harness, "joins", Some(MEMBER), None).await, 1);
}

#[tokio::test]
async fn a_counter_that_needs_a_user_refuses_a_view_without_one() {
    let harness = with(json!({ "strikes": { "per_user": true } }), json!({}));
    let replies = harness.invoke(command("view", "strikes")).await.unwrap();
    assert!(
        text_of(&replies).contains("user"),
        "the reply says a user is needed: {:?}",
        text_of(&replies)
    );
}

#[tokio::test]
async fn a_view_of_an_unknown_counter_says_so() {
    let harness = with(json!({ "strikes": { "per_user": true } }), json!({}));
    let replies = harness
        .invoke(scoped_to(command("view", "missing"), Some(MEMBER), None))
        .await
        .unwrap();
    assert!(text_of(&replies).contains("missing"));
}

// ---- commands ----

#[tokio::test]
async fn add_and_set_change_the_value_the_view_shows() {
    let harness = with(json!({ "strikes": { "per_user": true } }), json!({}));
    harness
        .invoke(scoped_to(
            command("set", "strikes").option("value", CommandOptionValue::Integer(4)),
            Some(MEMBER),
            None,
        ))
        .await
        .unwrap();
    harness
        .invoke(scoped_to(
            command("add", "strikes").option("value", CommandOptionValue::Integer(2)),
            Some(MEMBER),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(value(&harness, "strikes", Some(MEMBER), None).await, 6);
}

#[tokio::test]
async fn reset_puts_the_counter_back_to_its_initial_value() {
    let harness = with(
        json!({ "strikes": { "per_user": true, "initial_value": 1 } }),
        json!({}),
    );
    assert_eq!(value(&harness, "strikes", Some(MEMBER), None).await, 1);
    harness
        .invoke(scoped_to(
            command("set", "strikes").option("value", CommandOptionValue::Integer(9)),
            Some(MEMBER),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(value(&harness, "strikes", Some(MEMBER), None).await, 9);
    harness
        .invoke(scoped_to(command("reset", "strikes"), None, None))
        .await
        .unwrap();
    assert_eq!(value(&harness, "strikes", Some(MEMBER), None).await, 1);
}

#[tokio::test]
async fn the_counter_commands_need_the_permission() {
    let harness = with(json!({ "strikes": { "per_user": true } }), json!({}));
    let refused = harness
        .invoke(
            scoped_to(
                command("add", "strikes").option("value", CommandOptionValue::Integer(5)),
                Some(MEMBER),
                None,
            )
            .roles(Vec::<u64>::new()),
        )
        .await;
    assert!(
        refused.map_or(true, |replies| replies
            .iter()
            .all(|reply| !reply.content.contains("strikes ("))),
        "a member is not answered with the counter"
    );
    assert_eq!(
        value(&harness, "strikes", Some(MEMBER), None).await,
        0,
        "the refused add changed nothing"
    );
}

// ---- decay ----

#[tokio::test]
async fn decay_lowers_the_value_as_time_passes() {
    let harness = with(
        json!({ "strikes": { "per_user": true, "decay": { "amount": 1, "every": "1m" } } }),
        json!({}),
    );
    harness
        .invoke(scoped_to(
            command("set", "strikes").option("value", CommandOptionValue::Integer(5)),
            Some(MEMBER),
            None,
        ))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3 * 60));
    assert_eq!(value(&harness, "strikes", Some(MEMBER), None).await, 2);
}

// ---- triggers ----

#[tokio::test]
async fn a_trigger_fires_once_when_its_condition_newly_holds() {
    let harness = with(
        json!({ "strikes": { "per_user": true, "triggers": { "high": ">= 3" } } }),
        rules(&[
            (
                "plus",
                rule(
                    json!([words("+")]),
                    json!([{ "do": "add_to_counter", "counter": "strikes", "amount": 1 }]),
                ),
            ),
            (
                "high",
                rule(
                    json!([{ "type": "counter", "counter": "strikes", "trigger": "high" }]),
                    json!([alert("high")]),
                ),
            ),
        ]),
    );
    for _ in 0..3 {
        say(&harness, MEMBER, "+").await;
    }
    assert_eq!(texts_in(&harness, NOTIFY), vec!["high".to_owned()]);
    say(&harness, MEMBER, "+").await;
    assert_eq!(
        texts_in(&harness, NOTIFY),
        vec!["high".to_owned()],
        "a value that stays above the condition fires nothing"
    );
}

#[tokio::test]
async fn a_reverse_trigger_fires_when_the_value_falls_back_below() {
    let harness = with(
        json!({ "strikes": { "per_user": true, "triggers": { "high": ">= 3" } } }),
        rules(&[
            (
                "five",
                rule(
                    json!([words("five")]),
                    json!([{ "do": "set_counter", "counter": "strikes", "value": 5 }]),
                ),
            ),
            (
                "two",
                rule(
                    json!([words("two")]),
                    json!([{ "do": "set_counter", "counter": "strikes", "value": 2 }]),
                ),
            ),
            (
                "high",
                rule(
                    json!([{ "type": "counter", "counter": "strikes", "trigger": "high" }]),
                    json!([alert("high")]),
                ),
            ),
            (
                "low",
                rule(
                    json!([{ "type": "counter", "counter": "strikes", "trigger": "high", "reverse": true }]),
                    json!([alert("low")]),
                ),
            ),
        ]),
    );
    say(&harness, MEMBER, "five").await;
    say(&harness, MEMBER, "two").await;
    assert_eq!(
        texts_in(&harness, NOTIFY),
        vec!["high".to_owned(), "low".to_owned()]
    );
}

#[tokio::test]
async fn a_counter_trigger_runs_its_rule_and_that_rule_can_change_another_counter() {
    let harness = with(
        json!({
            "strikes": { "per_user": true, "triggers": { "high": ">= 1" } },
            "marks": { "per_user": true }
        }),
        rules(&[
            (
                "plus",
                rule(
                    json!([words("+")]),
                    json!([{ "do": "add_to_counter", "counter": "strikes", "amount": 1 }]),
                ),
            ),
            (
                "high",
                rule(
                    json!([{ "type": "counter", "counter": "strikes", "trigger": "high" }]),
                    json!([{ "do": "add_to_counter", "counter": "marks", "amount": 1 }]),
                ),
            ),
        ]),
    );
    say(&harness, MEMBER, "+").await;
    assert_eq!(value(&harness, "marks", Some(MEMBER), None).await, 1);
}

#[tokio::test]
async fn a_counter_chain_stops_at_the_depth_limit_and_says_so() {
    let harness = with(
        json!({ "x": { "per_user": true, "triggers": { "up": ">= 1" } } }),
        rules(&[
            (
                "plus",
                rule(
                    json!([words("+")]),
                    json!([{ "do": "add_to_counter", "counter": "x", "amount": 1 }]),
                ),
            ),
            (
                "down",
                rule(
                    json!([{ "type": "counter", "counter": "x", "trigger": "up" }]),
                    json!([{ "do": "set_counter", "counter": "x", "value": 0 }, alert("round")]),
                ),
            ),
            (
                "up_again",
                rule(
                    json!([{ "type": "counter", "counter": "x", "trigger": "up", "reverse": true }]),
                    json!([{ "do": "add_to_counter", "counter": "x", "amount": 1 }]),
                ),
            ),
        ]),
    );
    let result = harness
        .message(closure_sdk::testing::Message::new(MEMBER, CHANNEL).content("+"))
        .await;
    assert!(result.is_err(), "the chain that goes too deep is reported");
    assert_eq!(
        texts_in(&harness, NOTIFY),
        vec!["round".to_owned(), "round".to_owned()],
        "the rules run up to the depth limit and no further"
    );
}

#[tokio::test]
async fn a_trigger_of_an_unknown_counter_makes_its_rule_invalid_and_is_listed() {
    let harness = with(
        json!({ "strikes": { "per_user": true, "triggers": { "high": ">= 3" } } }),
        rules(&[(
            "bad",
            rule(
                json!([{ "type": "counter", "counter": "strikes", "trigger": "low" }]),
                json!([alert("x")]),
            ),
        )]),
    );
    let replies = harness
        .invoke(
            Invocation::new("automod")
                .subcommand("status")
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    let status = description(&replies);
    assert!(status.contains("1 invalid"), "{status}");
    assert!(
        status.contains("`bad`"),
        "the invalid rule is listed: {status}"
    );
}

#[tokio::test]
async fn a_counter_trigger_fired_before_a_failing_action_still_runs_its_rule() {
    let harness = with(
        json!({
            "strikes": { "per_user": true, "triggers": { "high": ">= 1" } },
            "seen": { "per_user": true }
        }),
        rules(&[
            (
                "plus",
                rule(
                    json!([words("+")]),
                    json!([
                        { "do": "add_to_counter", "counter": "strikes", "amount": 1 },
                        { "do": "delete" }
                    ]),
                ),
            ),
            (
                "high",
                rule(
                    json!([{ "type": "counter", "counter": "strikes", "trigger": "high" }]),
                    json!([{ "do": "add_to_counter", "counter": "seen", "amount": 1 }]),
                ),
            ),
        ]),
    );
    harness
        .api()
        .fail_nth(1, DiscordError::Fatal("the request was refused".into()));
    let result = harness
        .message(Message::new(MEMBER, CHANNEL).content("+"))
        .await;
    assert!(result.is_err(), "the failing delete is reported");
    assert_eq!(
        value(&harness, "seen", Some(MEMBER), None).await,
        1,
        "the counter change was stored, so its trigger runs its rule"
    );
}

#[tokio::test]
async fn the_counter_is_a_command_of_automod_and_the_old_group_is_gone() {
    let harness = with(json!({ "strikes": { "per_user": true } }), json!({}));
    let old = Invocation::new("automod-counters")
        .subcommand("view")
        .option("counter", CommandOptionValue::String("strikes".to_owned()))
        .roles([MODERATOR_ROLE]);
    let replies = harness.invoke(old).await.unwrap();
    assert!(replies.is_empty(), "the old top-level name is not answered");
    assert_eq!(value(&harness, "strikes", Some(MEMBER), None).await, 0);
}
