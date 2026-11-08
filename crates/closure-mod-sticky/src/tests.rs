use std::time::Duration;

use closure_core::action::Action;
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
