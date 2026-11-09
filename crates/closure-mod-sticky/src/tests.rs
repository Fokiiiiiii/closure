use std::time::Duration;

use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_sdk::testing::{Harness, Message};
use serde_json::json;
use twilight_model::id::Id;

use crate::builder;

const CHANNEL: u64 = 42;
const OTHER: u64 = 43;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness
        .api()
        .add_channel(Id::new(CHANNEL), Some(Id::new(1)));
    harness.set_config(json!({
        "stickies": [{ "channel": CHANNEL.to_string(), "text": "read the rules" }],
        "delay": "5s",
    }));
    harness
}

async fn settle(harness: &Harness) {
    harness.advance(Duration::from_secs(6));
    let ran = harness.run_due_jobs().await;
    assert!(ran.iter().all(Result::is_ok), "{ran:?}");
}

fn sent(harness: &Harness) -> usize {
    harness
        .actions()
        .iter()
        .filter(|action| matches!(action, Action::SendMessage { channel_id, .. } if channel_id.get() == CHANNEL))
        .count()
}

#[tokio::test]
async fn a_message_makes_the_text_come_again_after_the_delay() {
    let harness = harness();
    harness
        .message(Message::new(7, CHANNEL).content("hi"))
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 1);
    assert_eq!(sent(&harness), 0, "nothing before the delay");
    settle(&harness).await;
    assert_eq!(sent(&harness), 1);
    let last: Option<u64> = harness.store().get("last:42").await.unwrap();
    assert!(last.is_some());
}

#[tokio::test]
async fn a_burst_posts_once_and_the_old_message_is_deleted() {
    let harness = harness();
    for _ in 0..3 {
        harness
            .message(Message::new(7, CHANNEL).content("hi"))
            .await
            .unwrap();
    }
    assert_eq!(harness.jobs(), 1, "one waiting job per channel");
    settle(&harness).await;
    let first: u64 = harness.store().get("last:42").await.unwrap().unwrap();

    harness
        .message(Message::new(8, CHANNEL).content("again"))
        .await
        .unwrap();
    settle(&harness).await;
    assert_eq!(sent(&harness), 2);
    let second: u64 = harness.store().get("last:42").await.unwrap().unwrap();
    assert_ne!(first, second);
    assert!(harness.actions().iter().any(|action| matches!(
        action,
        Action::DeleteMessage { channel_id, message_id, .. }
            if channel_id.get() == CHANNEL && message_id.get() == first
    )));
}

#[tokio::test]
async fn bots_and_other_channels_do_not_move_the_text() {
    let harness = harness();
    harness
        .message(Message::new(9, CHANNEL).bot().content("beep"))
        .await
        .unwrap();
    harness
        .message(Message::new(7, OTHER).content("hi"))
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_text_taken_off_the_channel_is_not_posted() {
    let harness = harness();
    harness
        .message(Message::new(7, CHANNEL).content("hi"))
        .await
        .unwrap();
    harness.set_config(json!({ "stickies": [], "delay": "5s" }));
    settle(&harness).await;
    assert_eq!(sent(&harness), 0);
}

#[tokio::test]
async fn a_posted_text_is_listed_for_removal_from_the_start() {
    let harness = harness();
    harness
        .message(Message::new(7, CHANNEL).content("hi"))
        .await
        .unwrap();
    settle(&harness).await;
    let posted: u64 = harness.store().get("last:42").await.unwrap().unwrap();
    let listed: Vec<u64> = harness
        .store()
        .get("stale:42")
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(listed.contains(&posted));
}

#[tokio::test]
async fn an_old_message_whose_delete_fails_is_deleted_by_a_later_repost() {
    let harness = harness();
    harness
        .message(Message::new(7, CHANNEL).content("hi"))
        .await
        .unwrap();
    settle(&harness).await;
    let first: u64 = harness.store().get("last:42").await.unwrap().unwrap();

    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("no".into()));
    harness
        .message(Message::new(8, CHANNEL).content("again"))
        .await
        .unwrap();
    settle(&harness).await;
    let listed: Vec<u64> = harness
        .store()
        .get("stale:42")
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(
        listed.contains(&first),
        "a message whose delete failed stays listed"
    );

    harness
        .message(Message::new(9, CHANNEL).content("third"))
        .await
        .unwrap();
    settle(&harness).await;
    let listed: Vec<u64> = harness
        .store()
        .get("stale:42")
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(!listed.contains(&first));
    let deletes = harness
        .actions()
        .iter()
        .filter(|action| matches!(action, Action::DeleteMessage { message_id, .. } if message_id.get() == first))
        .count();
    assert_eq!(deletes, 1, "the next repost deletes it");
}

#[tokio::test]
async fn a_backlog_of_undeleted_messages_stops_new_posts() {
    let harness = harness();
    let backlog: Vec<u64> = (1000..1020).collect();
    harness.store().set("stale:42", &backlog).await.unwrap();
    harness
        .api()
        .fail_with(DiscordError::Forbidden("no".into()));
    harness
        .message(Message::new(7, CHANNEL).content("hi"))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(6));
    let ran = harness.run_due_jobs().await;
    assert_eq!(ran.len(), 1);
    assert!(ran.iter().all(Result::is_err), "{ran:?}");
    assert_eq!(sent(&harness), 0);
}
