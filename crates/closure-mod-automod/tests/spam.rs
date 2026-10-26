//! Spam triggers: a window of recent messages, measured, compared with an amount, and acted on
//! once per burst.
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

use closure_sdk::testing::Message;
use common::*;
use serde_json::{Value, json};

/// A rule `flood` with one spam trigger, the actions given.
fn flood(trigger: Value, actions: Value) -> closure_sdk::testing::Harness {
    with_rules(rules(&[("flood", rule(json!([trigger]), actions))]))
}

fn messages(amount: u32) -> Value {
    json!({ "type": "spam", "measure": "messages", "amount": amount, "within": "10s" })
}

// ---- the window ----

#[tokio::test]
async fn messages_within_the_window_trip_on_the_amount_and_delete_every_counted_message() {
    let harness = flood(messages(3), delete());
    say(&harness, MEMBER, "a").await;
    say(&harness, MEMBER, "b").await;
    assert_eq!(deletions(&harness), 0, "two messages are not a burst");
    say(&harness, MEMBER, "c").await;
    assert_eq!(
        deleted_ids(&harness).len(),
        3,
        "all three messages of the burst"
    );
}

#[tokio::test]
async fn messages_outside_the_window_do_not_add_up() {
    let harness = flood(messages(3), delete());
    say(&harness, MEMBER, "a").await;
    harness.advance(Duration::from_secs(11));
    say(&harness, MEMBER, "b").await;
    harness.advance(Duration::from_secs(11));
    say(&harness, MEMBER, "c").await;
    assert_eq!(deletions(&harness), 0);
}

#[tokio::test]
async fn per_channel_counts_each_channel_on_its_own() {
    let trigger = json!({
        "type": "spam", "measure": "messages", "amount": 3, "within": "10s", "per_channel": true
    });
    let harness = flood(trigger, delete());
    say(&harness, MEMBER, "a").await;
    say(&harness, MEMBER, "b").await;
    say_in(&harness, MEMBER, OTHER_CHANNEL, "c").await;
    assert_eq!(
        deletions(&harness),
        0,
        "two in one channel and one in another"
    );
    say(&harness, MEMBER, "d").await;
    assert_eq!(deletions(&harness), 3, "three in the same channel");
}

#[tokio::test]
async fn without_per_channel_the_channels_add_up() {
    let harness = flood(messages(3), delete());
    say(&harness, MEMBER, "a").await;
    say(&harness, MEMBER, "b").await;
    say_in(&harness, MEMBER, OTHER_CHANNEL, "c").await;
    assert_eq!(deletions(&harness), 3);
}

#[tokio::test]
async fn distinct_channels_needs_the_burst_to_span_that_many_channels() {
    let trigger = json!({
        "type": "spam", "measure": "messages", "amount": 3, "within": "10s", "distinct_channels": 2
    });
    let one_channel = flood(trigger.clone(), delete());
    for text in ["a", "b", "c"] {
        say(&one_channel, MEMBER, text).await;
    }
    assert_eq!(deletions(&one_channel), 0, "three messages in one channel");

    let two_channels = flood(trigger, delete());
    say(&two_channels, MEMBER, "a").await;
    say(&two_channels, MEMBER, "b").await;
    say_in(&two_channels, MEMBER, OTHER_CHANNEL, "c").await;
    assert_eq!(
        deleted_ids(&two_channels).len(),
        3,
        "spread over two channels"
    );
}

#[tokio::test]
async fn ignore_identical_messages_counts_a_repeated_text_once() {
    let trigger = json!({
        "type": "spam", "measure": "messages", "amount": 3, "within": "10s",
        "ignore_identical_messages": true
    });
    let repeated = flood(trigger.clone(), delete());
    for _ in 0..3 {
        say(&repeated, MEMBER, "same").await;
    }
    assert_eq!(deletions(&repeated), 0);

    let varied = flood(trigger, delete());
    for text in ["a", "b", "c"] {
        say(&varied, MEMBER, text).await;
    }
    assert_eq!(deletions(&varied), 3);
}

#[tokio::test]
async fn group_identical_messages_needs_every_counted_message_to_be_the_same() {
    let trigger = json!({
        "type": "spam", "measure": "messages", "amount": 3, "within": "10s",
        "group_identical_messages": true
    });
    let different = flood(trigger.clone(), delete());
    for text in ["a", "b", "c"] {
        say(&different, MEMBER, text).await;
    }
    assert_eq!(deletions(&different), 0);

    let same = flood(trigger, delete());
    for _ in 0..3 {
        say(&same, MEMBER, "same").await;
    }
    assert_eq!(deletions(&same), 3);
}

#[tokio::test]
async fn duplicates_counts_the_most_often_repeated_text() {
    let trigger = json!({ "type": "spam", "measure": "duplicates", "amount": 3, "within": "10s" });
    let repeated = flood(trigger.clone(), delete());
    for _ in 0..3 {
        say(&repeated, MEMBER, "same").await;
    }
    assert_eq!(deletions(&repeated), 3);

    let distinct = flood(trigger, delete());
    for text in ["a", "b", "c"] {
        say(&distinct, MEMBER, text).await;
    }
    assert_eq!(deletions(&distinct), 0);
}

// ---- the other measures ----

#[tokio::test]
async fn mentions_add_up_the_people_and_roles_each_message_mentions() {
    let trigger = json!({ "type": "spam", "measure": "mentions", "amount": 4, "within": "10s" });
    let harness = flood(trigger, delete());
    harness
        .message(
            Message::new(MEMBER, CHANNEL)
                .content("a")
                .mention(901)
                .mention(902),
        )
        .await
        .unwrap();
    assert_eq!(deletions(&harness), 0);
    harness
        .message(
            Message::new(MEMBER, CHANNEL)
                .content("b")
                .mention(903)
                .mention(904),
        )
        .await
        .unwrap();
    assert_eq!(deletions(&harness), 2, "both messages of the burst");
}

#[tokio::test]
async fn characters_of_one_message_count_without_a_window() {
    let trigger = json!({ "type": "spam", "measure": "characters", "amount": 20 });
    let long = deletes_when(trigger.clone());
    say(&long, MEMBER, &"x".repeat(25)).await;
    assert_eq!(deletions(&long), 1);

    let short = deletes_when(trigger);
    say(&short, MEMBER, &"x".repeat(19)).await;
    assert_eq!(deletions(&short), 0);
}

#[tokio::test]
async fn links_add_up_the_links_of_each_message() {
    let trigger = json!({ "type": "spam", "measure": "links", "amount": 2, "within": "10s" });
    let harness = flood(trigger, delete());
    say(&harness, MEMBER, "https://a.example/1").await;
    assert_eq!(deletions(&harness), 0);
    say(&harness, MEMBER, "https://a.example/2").await;
    assert_eq!(deletions(&harness), 2);
}

#[tokio::test]
async fn attachments_add_up_the_files_of_each_message() {
    let trigger = json!({ "type": "spam", "measure": "attachments", "amount": 2, "within": "10s" });
    let harness = flood(trigger, delete());
    harness
        .message(Message::new(MEMBER, CHANNEL).attachment("a.png", 10))
        .await
        .unwrap();
    assert_eq!(deletions(&harness), 0);
    harness
        .message(Message::new(MEMBER, CHANNEL).attachment("b.png", 10))
        .await
        .unwrap();
    assert_eq!(deletions(&harness), 2);
}

#[tokio::test]
async fn emojis_add_up_the_emoji_of_each_message() {
    let trigger = json!({ "type": "spam", "measure": "emojis", "amount": 4, "within": "10s" });
    let harness = flood(trigger, delete());
    say(&harness, MEMBER, "😀😀").await;
    assert_eq!(deletions(&harness), 0);
    say(&harness, MEMBER, "😀😀").await;
    assert_eq!(deletions(&harness), 2, "both messages of the burst");
}

#[tokio::test]
async fn lines_add_up_the_lines_of_each_message() {
    let trigger = json!({ "type": "spam", "measure": "lines", "amount": 6, "within": "10s" });
    let harness = flood(trigger, delete());
    say(&harness, MEMBER, "a\nb\nc").await;
    assert_eq!(deletions(&harness), 0);
    say(&harness, MEMBER, "a\nb\nc").await;
    assert_eq!(deletions(&harness), 2, "both messages of the burst");
}

#[tokio::test]
async fn stickers_add_up_the_stickers_of_each_message() {
    let trigger = json!({ "type": "spam", "measure": "stickers", "amount": 4, "within": "10s" });
    let harness = flood(trigger, delete());
    for _ in 0..2 {
        let mut info = custom(MEMBER, CHANNEL, "");
        info.sticker_count = 2;
        post(&harness, info).await;
    }
    assert_eq!(deletions(&harness), 2);
}

// ---- one burst, one hit ----

#[tokio::test]
async fn a_burst_fires_once_and_its_later_messages_are_removed_without_a_second_punishment() {
    let harness = flood(messages(3), json!([{ "do": "delete" }, { "do": "warn" }]));
    for text in ["a", "b", "c"] {
        say(&harness, MEMBER, text).await;
    }
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
    assert_eq!(deleted_ids(&harness).len(), 3);

    say(&harness, MEMBER, "d").await;
    say(&harness, MEMBER, "e").await;
    assert_eq!(
        cases_of(&harness, MEMBER).await.len(),
        1,
        "no second punishment for the same burst"
    );
    assert_eq!(
        deleted_ids(&harness).len(),
        5,
        "the messages that join the burst are removed too"
    );
}

#[tokio::test]
async fn after_the_window_a_new_burst_fires_again() {
    let harness = flood(messages(3), json!([{ "do": "warn" }]));
    for text in ["a", "b", "c"] {
        say(&harness, MEMBER, text).await;
    }
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);

    harness.advance(Duration::from_secs(11));
    for text in ["d", "e", "f"] {
        say(&harness, MEMBER, text).await;
    }
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 2);
}

#[tokio::test]
async fn a_burst_of_one_member_does_not_count_for_another() {
    let harness = flood(messages(3), delete());
    say(&harness, MEMBER, "a").await;
    say(&harness, OTHER_MEMBER, "b").await;
    say(&harness, MEMBER, "c").await;
    say(&harness, OTHER_MEMBER, "d").await;
    assert_eq!(deletions(&harness), 0);
}

#[tokio::test]
async fn a_single_message_measure_without_a_window_needs_no_history() {
    let harness = flood(
        json!({ "type": "spam", "measure": "lines", "amount": 3 }),
        delete(),
    );
    say(&harness, MEMBER, "a\nb\nc").await;
    assert_eq!(deletions(&harness), 1);
}
