//! The events that are not messages: joins and leaves, role changes, cases and threads. Each
//! trigger hits on its own event and misses the others, the actions that need a message are
//! skipped without one, and the role, nickname and slowmode actions make their Discord calls.
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

use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_sdk::testing::Message;
use common::*;
use serde_json::{Value, json};
use twilight_model::id::Id;

const OTHER_ROLE: u64 = 78;
/// The owner of the pretend server, as the harness names it when a test does not.
const OWNER: u64 = 999_999_999;
const THREAD: u64 = 90;

fn notify(text: &str) -> Value {
    json!({ "do": "alert", "channel": NOTIFY.to_string(), "text": text })
}

fn alerting(trigger: Value, text: &str) -> Value {
    rule(json!([trigger]), json!([notify(text)]))
}

fn bans_of(harness: &closure_sdk::testing::Harness) -> Vec<u64> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::Ban { user_id, .. } => Some(user_id.get()),
            _ => None,
        })
        .collect()
}

fn count_kicks(harness: &closure_sdk::testing::Harness) -> usize {
    count_actions(harness, |a| matches!(a, Action::Kick { .. }))
}

// ---- joins and leaves ----

#[tokio::test]
async fn member_join_hits_every_join_without_only_new() {
    let harness = with_rules(rules(&[(
        "joins",
        rule(
            json!([{ "type": "member_join" }]),
            json!([{ "do": "kick" }]),
        ),
    )]));
    join(&harness, MEMBER).await;
    join(&harness, OTHER_MEMBER).await;
    assert_eq!(count_kicks(&harness), 2);
}

#[tokio::test]
async fn member_join_only_new_hits_a_young_account_and_misses_an_old_one() {
    let harness = with_rules(rules(&[(
        "young",
        rule(
            json!([{ "type": "member_join", "only_new": true, "new_threshold": "1h" }]),
            json!([{ "do": "kick" }]),
        ),
    )]));
    join(&harness, MEMBER).await; // id 7: an account from 2015
    assert_eq!(count_kicks(&harness), 0, "an old account is not new");
    join(&harness, account_made(10 * 60)).await;
    assert_eq!(
        count_kicks(&harness),
        1,
        "an account made ten minutes ago is"
    );
}

#[tokio::test]
async fn member_join_spam_bans_every_member_counted_in_the_window() {
    let harness = with_rules(rules(&[(
        "raid",
        rule(
            json!([{ "type": "member_join_spam", "amount": 3, "within": "1m" }]),
            json!([{ "do": "ban" }]),
        ),
    )]));
    join(&harness, MEMBER).await;
    join(&harness, OTHER_MEMBER).await;
    assert!(bans_of(&harness).is_empty(), "two joins are not three");
    join(&harness, 11).await;
    let mut banned = bans_of(&harness);
    banned.sort_unstable();
    assert_eq!(
        banned,
        vec![MEMBER, OTHER_MEMBER, 11],
        "the third join bans all three members counted"
    );
    join(&harness, 12).await;
    assert_eq!(
        bans_of(&harness).len(),
        4,
        "a member who joins during the burst is banned as well"
    );
}

#[tokio::test]
async fn member_join_spam_misses_joins_outside_the_window() {
    let harness = with_rules(rules(&[(
        "raid",
        rule(
            json!([{ "type": "member_join_spam", "amount": 2, "within": "10s" }]),
            json!([{ "do": "ban" }]),
        ),
    )]));
    join(&harness, MEMBER).await;
    harness.advance(Duration::from_secs(30));
    join(&harness, OTHER_MEMBER).await;
    assert!(bans_of(&harness).is_empty());
}

#[tokio::test]
async fn member_leave_hits_a_departure() {
    let harness = with_rules(rules(&[(
        "left",
        alerting(json!({ "type": "member_leave" }), "left {username}"),
    )]));
    harness
        .api()
        .set_not_a_member(Id::new(GUILD), Id::new(MEMBER));
    leave(&harness, MEMBER).await;
    assert_eq!(texts_in(&harness, NOTIFY), vec!["left member".to_owned()]);
}

// ---- role changes ----

#[tokio::test]
async fn role_added_hits_when_one_of_its_roles_is_added() {
    let harness = with_rules(rules(&[(
        "got",
        alerting(
            json!({ "type": "role_added", "roles": [ROLE.to_string()] }),
            "got",
        ),
    )]));
    declare(&harness, MEMBER, &[]);
    harness
        .roles_changed(MEMBER, &[OTHER_ROLE], &[])
        .await
        .unwrap();
    assert!(
        texts_in(&harness, NOTIFY).is_empty(),
        "another role is not one of them"
    );
    harness.roles_changed(MEMBER, &[ROLE], &[]).await.unwrap();
    assert_eq!(texts_in(&harness, NOTIFY), vec!["got".to_owned()]);
}

#[tokio::test]
async fn role_removed_misses_an_added_role_and_hits_a_removed_one() {
    let harness = with_rules(rules(&[(
        "lost",
        alerting(
            json!({ "type": "role_removed", "roles": [ROLE.to_string()] }),
            "lost",
        ),
    )]));
    declare(&harness, MEMBER, &[]);
    harness.roles_changed(MEMBER, &[ROLE], &[]).await.unwrap();
    assert!(texts_in(&harness, NOTIFY).is_empty());
    harness.roles_changed(MEMBER, &[], &[ROLE]).await.unwrap();
    assert_eq!(texts_in(&harness, NOTIFY), vec!["lost".to_owned()]);
}

#[tokio::test]
async fn a_role_the_bot_gave_itself_is_not_counted_once() {
    let harness = with_rules(rules(&[
        (
            "give",
            rule(
                json!([{ "type": "words", "words": ["give"] }]),
                json!([{ "do": "add_roles", "roles": [ROLE.to_string()] }]),
            ),
        ),
        (
            "counted",
            alerting(
                json!({ "type": "role_added", "roles": [ROLE.to_string()] }),
                "counted",
            ),
        ),
    ]));
    declare(&harness, MEMBER, &[]);
    say(&harness, MEMBER, "give").await;
    harness.roles_changed(MEMBER, &[ROLE], &[]).await.unwrap();
    assert!(
        texts_in(&harness, NOTIFY).is_empty(),
        "the change the bot made is ignored"
    );
    harness.roles_changed(MEMBER, &[ROLE], &[]).await.unwrap();
    assert_eq!(
        texts_in(&harness, NOTIFY),
        vec!["counted".to_owned()],
        "the next change, made by someone else, counts"
    );
}

#[tokio::test]
async fn a_role_the_bot_gave_itself_counts_again_after_ten_seconds() {
    let harness = with_rules(rules(&[
        (
            "give",
            rule(
                json!([{ "type": "words", "words": ["give"] }]),
                json!([{ "do": "add_roles", "roles": [ROLE.to_string()] }]),
            ),
        ),
        (
            "counted",
            alerting(
                json!({ "type": "role_added", "roles": [ROLE.to_string()] }),
                "counted",
            ),
        ),
    ]));
    declare(&harness, MEMBER, &[]);
    say(&harness, MEMBER, "give").await;
    harness.advance(Duration::from_secs(11));
    harness.roles_changed(MEMBER, &[ROLE], &[]).await.unwrap();
    assert_eq!(texts_in(&harness, NOTIFY), vec!["counted".to_owned()]);
}

// ---- cases ----

#[tokio::test]
async fn a_case_of_the_bot_itself_is_ignored_unless_include_own_is_set() {
    let ignoring = with_rules(rules(&[(
        "cases",
        alerting(json!({ "type": "case" }), "case"),
    )]));
    declare(&ignoring, MEMBER, &[]);
    ignoring
        .case_created(case("automod", MEMBER, BOT))
        .await
        .unwrap();
    assert!(
        texts_in(&ignoring, NOTIFY).is_empty(),
        "the bot's own case is ignored"
    );

    let including = with_rules(rules(&[(
        "cases",
        alerting(json!({ "type": "case", "include_own": true }), "case"),
    )]));
    declare(&including, MEMBER, &[]);
    including
        .case_created(case("automod", MEMBER, BOT))
        .await
        .unwrap();
    assert_eq!(texts_in(&including, NOTIFY), vec!["case".to_owned()]);
}

#[tokio::test]
async fn a_case_trigger_hits_only_the_kinds_it_lists() {
    let harness = with_rules(rules(&[(
        "bans",
        alerting(json!({ "type": "case", "kinds": ["ban"] }), "ban"),
    )]));
    declare(&harness, MEMBER, &[]);
    harness
        .case_created(case("warn", MEMBER, MODERATOR))
        .await
        .unwrap();
    assert!(texts_in(&harness, NOTIFY).is_empty());
    harness
        .case_created(case("ban", MEMBER, MODERATOR))
        .await
        .unwrap();
    assert_eq!(texts_in(&harness, NOTIFY), vec!["ban".to_owned()]);
}

// ---- threads ----

#[tokio::test]
async fn each_thread_trigger_hits_only_its_own_event() {
    let harness = with_rules(rules(&[
        (
            "created",
            alerting(json!({ "type": "thread_created" }), "created"),
        ),
        (
            "archived",
            alerting(json!({ "type": "thread_archived" }), "archived"),
        ),
        (
            "unarchived",
            alerting(json!({ "type": "thread_unarchived" }), "unarchived"),
        ),
        (
            "deleted",
            alerting(json!({ "type": "thread_deleted" }), "deleted"),
        ),
    ]));
    declare(&harness, OWNER, &[]);
    harness.thread_created(THREAD, CHANNEL).await.unwrap();
    assert_eq!(texts_in(&harness, NOTIFY), vec!["created".to_owned()]);
    harness.thread_archived(THREAD, CHANNEL).await.unwrap();
    harness.thread_unarchived(THREAD, CHANNEL).await.unwrap();
    harness.thread_deleted(THREAD, CHANNEL).await.unwrap();
    assert_eq!(
        texts_in(&harness, NOTIFY),
        vec!["created", "archived", "unarchived", "deleted"]
    );
}

// ---- without a message ----

#[tokio::test]
async fn actions_that_need_a_message_are_skipped_silently_for_a_join() {
    let harness = with_rules(rules(&[(
        "joins",
        rule(
            json!([{ "type": "member_join" }]),
            json!([
                { "do": "delete" },
                { "do": "reply", "text": "hello" },
                { "do": "alert", "channel": NOTIFY.to_string(), "text": "joined {username}" }
            ]),
        ),
    )]));
    join(&harness, MEMBER).await;
    assert_eq!(deletions(&harness), 0, "there is no message to delete");
    assert!(
        sent_texts(&harness).iter().all(|(_, text)| text != "hello"),
        "there is no message to reply to"
    );
    assert_eq!(texts_in(&harness, NOTIFY), vec!["joined member".to_owned()]);
}

#[tokio::test]
async fn a_punishment_of_a_join_targets_the_joining_member() {
    let harness = with_rules(rules(&[(
        "joins",
        rule(
            json!([{ "type": "member_join" }]),
            json!([{ "do": "warn" }]),
        ),
    )]));
    join(&harness, MEMBER).await;
    assert_eq!(cases_of(&harness, MEMBER).await[0].kind.as_str(), "warn");
}

// ---- role, nickname and slowmode actions ----

#[tokio::test]
async fn add_roles_adds_each_role_to_the_member() {
    let harness = with_rules(rules(&[(
        "roles",
        rule(
            json!([{ "type": "words", "words": ["roles"] }]),
            json!([{ "do": "add_roles", "roles": [ROLE.to_string(), OTHER_ROLE.to_string()] }]),
        ),
    )]));
    say(&harness, MEMBER, "roles").await;
    let added: Vec<u64> = harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::AddRole {
                user_id, role_id, ..
            } if *user_id == Id::new(MEMBER) => Some(role_id.get()),
            _ => None,
        })
        .collect();
    assert_eq!(added, vec![ROLE, OTHER_ROLE]);
}

#[tokio::test]
async fn remove_roles_removes_each_role_from_the_member() {
    let harness = with_rules(rules(&[(
        "roles",
        rule(
            json!([{ "type": "words", "words": ["roles"] }]),
            json!([{ "do": "remove_roles", "roles": [ROLE.to_string()] }]),
        ),
    )]));
    say(&harness, MEMBER, "roles").await;
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::RemoveRole { role_id, .. } if *role_id == Id::new(ROLE)
        )),
        1
    );
}

#[tokio::test]
async fn set_nickname_renders_its_template_for_the_member() {
    let harness = with_rules(rules(&[(
        "nick",
        rule(
            json!([{ "type": "words", "words": ["nick"] }]),
            json!([{ "do": "set_nickname", "name": "{username} (checked)" }]),
        ),
    )]));
    say(&harness, MEMBER, "nick").await;
    let nickname = harness
        .api()
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::SetNickname { nickname, .. } => nickname,
            _ => None,
        });
    assert_eq!(nickname.as_deref(), Some("tester (checked)"));
}

#[tokio::test]
async fn slowmode_sets_the_channel_of_the_hit_and_skips_an_identical_repeat() {
    let harness = with_rules(rules(&[(
        "slow",
        rule(
            json!([{ "type": "words", "words": ["slow"] }]),
            json!([{ "do": "slowmode", "duration": "30s" }]),
        ),
    )]));
    say(&harness, MEMBER, "slow").await;
    say(&harness, MEMBER, "slow").await;
    let edits: Vec<(Id<twilight_model::id::marker::ChannelMarker>, Option<u16>)> = harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::EditChannel {
                channel_id, patch, ..
            } => Some((channel_id, patch.slowmode_seconds)),
            _ => None,
        })
        .collect();
    assert_eq!(edits, vec![(Id::new(CHANNEL), Some(30))]);
}

#[tokio::test]
async fn slowmode_names_its_channels_when_it_is_given_them() {
    let harness = with_rules(rules(&[(
        "slow",
        rule(
            json!([{ "type": "words", "words": ["slow"] }]),
            json!([{ "do": "slowmode", "duration": "1m", "channels": [OTHER_CHANNEL.to_string()] }]),
        ),
    )]));
    say(&harness, MEMBER, "slow").await;
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::EditChannel { channel_id, .. } if *channel_id == Id::new(OTHER_CHANNEL)
        )),
        1
    );
}

#[tokio::test]
async fn slowmode_on_a_join_without_a_channel_does_nothing() {
    let harness = with_rules(rules(&[(
        "joins",
        rule(
            json!([{ "type": "member_join" }]),
            json!([{ "do": "slowmode", "duration": "30s" }]),
        ),
    )]));
    join(&harness, MEMBER).await;
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::EditChannel { .. })),
        0
    );
}

// ---- missing permissions ----

#[tokio::test]
async fn a_forbidden_action_is_reported_once_a_minute_and_a_real_failure_always_is() {
    let harness = with_rules(rules(&[(
        "give",
        rule(
            json!([{ "type": "words", "words": ["give"] }]),
            json!([{ "do": "add_roles", "roles": [ROLE.to_string()] }]),
        ),
    )]));
    harness
        .api()
        .fail_with(DiscordError::Forbidden("missing Manage Roles".into()));
    let mut errors = 0;
    for _ in 0..3 {
        let posted = harness
            .message(Message::new(MEMBER, CHANNEL).content("give"))
            .await;
        if posted.is_err() {
            errors += 1;
        }
    }
    assert_eq!(errors, 1, "one report in the minute, not one per message");
    harness.advance(Duration::from_secs(61));
    assert!(
        harness
            .message(Message::new(MEMBER, CHANNEL).content("give"))
            .await
            .is_err(),
        "the next minute reports it again"
    );

    let broken = with_rules(rules(&[(
        "give",
        rule(
            json!([{ "type": "words", "words": ["give"] }]),
            json!([{ "do": "add_roles", "roles": [ROLE.to_string()] }]),
        ),
    )]));
    broken
        .api()
        .fail_with(DiscordError::Fatal("the request was refused".into()));
    for _ in 0..2 {
        assert!(
            broken
                .message(Message::new(MEMBER, CHANNEL).content("give"))
                .await
                .is_err(),
            "a failure that is not a missing permission is reported every time"
        );
    }
}
