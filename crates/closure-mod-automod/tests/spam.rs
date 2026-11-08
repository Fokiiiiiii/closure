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

use std::sync::Arc;
use std::time::Duration;

use closure_sdk::testing::{Harness, Message};
use common::*;
use serde_json::{Value, json};

fn flood(trigger: Value, actions: Value) -> closure_sdk::testing::Harness {
    with_rules(rules(&[("flood", rule(json!([trigger]), actions))]))
}

fn messages(amount: u32) -> Value {
    json!({ "type": "spam", "measure": "messages", "amount": amount, "within": "10s" })
}

fn coordinated(measure: &str) -> Value {
    json!({ "type": "spam", "measure": measure, "amount": 3, "within": "10s", "distinct_users": 3 })
}

#[tokio::test]
async fn coordinated_spam_targets_every_user_once() {
    let harness = flood(
        coordinated("messages"),
        json!([{ "do": "delete" }, { "do": "warn" }]),
    );
    for user in [MEMBER, OTHER_MEMBER, 11] {
        declare(&harness, user, &[]);
        say(&harness, user, "same content").await;
    }
    assert_eq!(deletions(&harness), 3);
    for user in [MEMBER, OTHER_MEMBER, 11] {
        assert_eq!(cases_of(&harness, user).await.len(), 1);
    }
    say(&harness, MEMBER, "same content").await;
    assert_eq!(deletions(&harness), 4);
    for user in [MEMBER, OTHER_MEMBER, 11] {
        assert_eq!(cases_of(&harness, user).await.len(), 1);
    }
}

#[tokio::test]
async fn coordinated_spam_requires_distinct_users_and_matching_content() {
    for posts in [
        vec![(MEMBER, "same"), (OTHER_MEMBER, "same"), (MEMBER, "same")],
        vec![(MEMBER, "same"), (MEMBER, "same"), (MEMBER, "same")],
        vec![(MEMBER, "one"), (OTHER_MEMBER, "two"), (11, "three")],
    ] {
        let harness = flood(coordinated("messages"), delete());
        for (user, content) in posts {
            say(&harness, user, content).await;
        }
        assert_eq!(deletions(&harness), 0);
    }
}

#[tokio::test]
async fn coordinated_link_spam_compares_link_sets() {
    let harness = flood(coordinated("links"), delete());
    say(&harness, MEMBER, "first https://a.example/x").await;
    say(&harness, OTHER_MEMBER, "different https://b.example/x").await;
    say(&harness, 11, "another https://a.example/x").await;
    assert_eq!(deletions(&harness), 0);
    say(&harness, 12, "yet another https://a.example/x").await;
    assert_eq!(deletions(&harness), 3);
}

#[tokio::test]
async fn coordinated_link_spam_counts_messages_and_ignores_link_order() {
    let harness = flood(coordinated("links"), delete());
    say(&harness, MEMBER, "https://a.example https://b.example").await;
    say(
        &harness,
        OTHER_MEMBER,
        "https://b.example https://a.example",
    )
    .await;
    assert_eq!(deletions(&harness), 0);
    say(
        &harness,
        11,
        "https://a.example https://b.example https://a.example",
    )
    .await;
    assert_eq!(deletions(&harness), 3);
}

#[tokio::test]
async fn coordinated_spam_keeps_channel_and_identical_options() {
    let mut trigger = coordinated("messages");
    trigger["per_channel"] = json!(true);
    trigger["group_identical_messages"] = json!(true);
    let harness = flood(trigger, delete());
    say(&harness, MEMBER, "same").await;
    say(&harness, OTHER_MEMBER, "same").await;
    say_in(&harness, 11, OTHER_CHANNEL, "same").await;
    assert_eq!(deletions(&harness), 0);
    say(&harness, 12, "same").await;
    assert_eq!(deletions(&harness), 3);

    let mut trigger = coordinated("messages");
    trigger["ignore_identical_messages"] = json!(true);
    let harness = flood(trigger, delete());
    for user in [MEMBER, OTHER_MEMBER, 11] {
        say(&harness, user, "same").await;
    }
    assert_eq!(deletions(&harness), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coordinated_spam_concurrent_messages_fire_once() {
    for _ in 0..20 {
        let harness = Arc::new(flood(
            coordinated("messages"),
            json!([{ "do": "alert", "channel": NOTIFY.to_string(), "text": "burst" }]),
        ));
        let tasks = [MEMBER, OTHER_MEMBER, 11].map(|user| {
            let harness = Arc::clone(&harness);
            tokio::spawn(async move { post(&harness, custom(user, CHANNEL, "same")).await })
        });
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(texts_in(&harness, NOTIFY), vec!["burst".to_owned()]);
    }
}

#[tokio::test]
async fn coordinated_spam_excludes_moderators_and_expired_messages() {
    let harness = flood(coordinated("messages"), delete());
    let mut moderator = custom(MODERATOR, CHANNEL, "same");
    moderator.author_roles = vec![twilight_model::id::Id::new(MODERATOR_ROLE)];
    post(&harness, moderator).await;
    say(&harness, MEMBER, "same").await;
    say(&harness, OTHER_MEMBER, "same").await;
    assert_eq!(deletions(&harness), 0);
    harness.advance(Duration::from_secs(11));
    say(&harness, 11, "same").await;
    assert_eq!(deletions(&harness), 0);
    say(&harness, 12, "same").await;
    say(&harness, 13, "same").await;
    assert_eq!(deletions(&harness), 3);
}

#[tokio::test]
async fn coordinated_spam_dry_run_does_not_act() {
    let harness = configured(
        json!({ "dry_run": true, "rules": rules(&[("flood", rule(json!([coordinated("messages")]), json!([{ "do": "delete" }, { "do": "warn" }])) )]) }),
    );
    for user in [MEMBER, OTHER_MEMBER, 11] {
        say(&harness, user, "same").await;
    }
    assert_eq!(deletions(&harness), 0);
    for user in [MEMBER, OTHER_MEMBER, 11] {
        assert!(cases_of(&harness, user).await.is_empty());
    }
}

#[tokio::test]
async fn coordinated_spam_continues_when_a_target_has_left() {
    use closure_core::action::Action;
    use closure_sdk::DiscordError;
    use std::io::{self, Write};
    use std::sync::Mutex;
    use twilight_model::id::Id;

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let captured = Capture(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer({
            let captured = captured.clone();
            move || captured.clone()
        })
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let harness = configured(json!({
        "dm_on_action": false,
        "rules": rules(&[("flood", rule(json!([coordinated("messages")]), json!([{ "do": "kick" }])))])
    }));
    for user in [MEMBER, OTHER_MEMBER, 11] {
        declare(&harness, user, &[]);
    }
    say(&harness, MEMBER, "same").await;
    say(&harness, OTHER_MEMBER, "same").await;
    harness.api().fail_nth(1, DiscordError::NotFound);
    let result = harness
        .message(Message::new(11, CHANNEL).content("same"))
        .await;
    assert!(result.is_err());
    let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("automod action failed for target") && log.contains("user=7"),
        "{log}"
    );
    for user in [OTHER_MEMBER, 11] {
        assert!(harness.api().actions().iter().any(
            |action| matches!(action, Action::Kick { user_id, .. } if *user_id == Id::new(user))
        ));
    }
}

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
async fn group_identical_messages_does_not_take_attachment_only_posts_for_a_text() {
    let trigger = json!({
        "type": "spam", "measure": "messages", "amount": 3, "within": "10s",
        "group_identical_messages": true
    });
    let harness = flood(trigger, delete());
    say(&harness, MEMBER, "hello").await;
    for _ in 0..2 {
        harness
            .message(Message::new(MEMBER, CHANNEL).attachment("a.png", 10))
            .await
            .unwrap();
    }
    assert_eq!(
        deletions(&harness),
        0,
        "two posts without text are not the same text as hello"
    );
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

fn alerts_on_two() -> Harness {
    flood(
        messages(2),
        json!([{ "do": "alert", "channel": NOTIFY.to_string(), "text": "burst" }]),
    )
}

fn alerts_on_two_joins() -> Harness {
    flood(
        json!({ "type": "member_join_spam", "amount": 2, "within": "30s" }),
        json!([{ "do": "alert", "channel": NOTIFY.to_string(), "text": "burst" }]),
    )
}

#[tokio::test]
async fn two_evaluations_of_one_burst_punish_once() {
    let harness = alerts_on_two();
    tokio::join!(
        post(&harness, custom(MEMBER, CHANNEL, "a")),
        post(&harness, custom(MEMBER, CHANNEL, "b")),
    );
    assert_eq!(texts_in(&harness, NOTIFY), vec!["burst".to_owned()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_messages_of_one_burst_on_two_threads_punish_once() {
    for round in 0..200 {
        let harness = Arc::new(alerts_on_two());
        let tasks = ["a", "b"].map(|text| {
            let harness = Arc::clone(&harness);
            tokio::spawn(async move { post(&harness, custom(MEMBER, CHANNEL, text)).await })
        });
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(
            texts_in(&harness, NOTIFY),
            vec!["burst".to_owned()],
            "round {round}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_joins_of_one_join_burst_on_two_threads_punish_once() {
    for round in 0..200 {
        let harness = Arc::new(alerts_on_two_joins());
        let tasks = [MEMBER, OTHER_MEMBER].map(|user| {
            let harness = Arc::clone(&harness);
            tokio::spawn(async move { join(&harness, user).await })
        });
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(
            texts_in(&harness, NOTIFY),
            vec!["burst".to_owned()],
            "round {round}"
        );
    }
}

#[tokio::test]
async fn thread_create_spam_counts_each_owner_within_the_window() {
    use closure_core::event::ThreadInfo;
    use closure_sdk::Event;
    use twilight_model::id::Id;

    let harness = flood(
        json!({ "type": "thread_create_spam", "amount": 2, "within": "30s" }),
        json!([{ "do": "warn" }]),
    );
    declare(&harness, MEMBER, &[]);
    declare(&harness, OTHER_MEMBER, &[]);
    let emit = |id, owner: Option<u64>| {
        Event::ThreadCreated(ThreadInfo {
            id: Id::new(id),
            guild_id: Id::new(GUILD),
            parent_id: Some(Id::new(CHANNEL)),
            name: String::new(),
            owner_id: owner.map(Id::new),
            archived: false,
            locked: false,
            created_at: None,
        })
    };
    harness.emit(emit(100, Some(MEMBER))).await.unwrap();
    harness.emit(emit(101, Some(OTHER_MEMBER))).await.unwrap();
    harness.emit(emit(102, None)).await.unwrap();
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    harness.emit(emit(103, Some(MEMBER))).await.unwrap();
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
    assert!(cases_of(&harness, OTHER_MEMBER).await.is_empty());
    harness.advance(std::time::Duration::from_secs(31));
    harness.emit(emit(104, Some(OTHER_MEMBER))).await.unwrap();
    assert!(cases_of(&harness, OTHER_MEMBER).await.is_empty());
}

#[tokio::test]
async fn spam_can_count_threads_as_their_parent_channel() {
    for enabled in [false, true] {
        let harness = flood(
            json!({ "type": "spam", "measure": "messages", "amount": 2, "within": "30s",
                "per_channel": true, "threads_as_parent_channel": enabled }),
            delete(),
        );
        declare(&harness, 999_999_999, &[]);
        for channel in [90, 91] {
            harness.api().add_channel(
                twilight_model::id::Id::new(channel),
                Some(twilight_model::id::Id::new(GUILD)),
            );
        }
        harness.thread_created(90, CHANNEL).await.unwrap();
        harness.thread_created(91, CHANNEL).await.unwrap();
        say_in(&harness, MEMBER, 90, "one").await;
        say_in(&harness, MEMBER, 91, "two").await;
        assert_eq!(
            deletions(&harness),
            if enabled { 2 } else { 0 },
            "{:?}",
            harness.api().actions()
        );
    }
    let harness = flood(
        json!({ "type": "spam", "measure": "messages", "amount": 2, "within": "30s",
            "distinct_channels": 2, "threads_as_parent_channel": true }),
        delete(),
    );
    declare(&harness, 999_999_999, &[]);
    for channel in [90, 91, 92] {
        harness.api().add_channel(
            twilight_model::id::Id::new(channel),
            Some(twilight_model::id::Id::new(GUILD)),
        );
    }
    harness.thread_created(90, CHANNEL).await.unwrap();
    harness.thread_created(91, CHANNEL).await.unwrap();
    say_in(&harness, MEMBER, 90, "one").await;
    say_in(&harness, MEMBER, 91, "two").await;
    assert_eq!(deletions(&harness), 0);
    say_in(&harness, MEMBER, 92, "unknown thread").await;
    assert_eq!(deletions(&harness), 3);
}
