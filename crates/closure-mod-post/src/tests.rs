use std::time::Duration;

use closure_core::action::Action;
use closure_core::event::{ChannelId, GuildId};
use closure_sdk::testing::Harness;
use serde_json::json;

use crate::builder;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness
        .api()
        .add_channel(ChannelId::new(5), Some(GuildId::new(1)));
    harness
}

async fn schedule(harness: &Harness, payload: serde_json::Value) {
    harness
        .scheduler()
        .job("send", &payload)
        .unwrap()
        .after(Duration::from_secs(30))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_scheduled_post_is_sent_with_its_buttons_when_its_time_comes() {
    let harness = harness();
    schedule(
        &harness,
        json!({
            "channel": "5",
            "message": {"content": "pick", "buttons": [{"label": "News", "role": "9"}]},
        }),
    )
    .await;
    harness.run_due_jobs().await;
    assert!(harness.actions().is_empty(), "not yet");
    harness.advance(Duration::from_secs(31));
    harness.run_due_jobs().await;
    let posted = harness.actions().iter().any(|a| {
        matches!(a, Action::PostMessage { channel_id, message_id: None, content: Some(c), components, .. }
            if channel_id.get() == 5 && c == "pick" && components.len() == 1)
    });
    assert!(posted);
}

#[tokio::test]
async fn a_scheduled_edit_replaces_the_message_and_a_broken_post_is_dropped() {
    let harness = harness();
    schedule(
        &harness,
        json!({"channel": "5", "edit": "77", "message": {"content": "new"}}),
    )
    .await;
    schedule(
        &harness,
        json!({"channel": "5", "message": {"content": "", "embeds": []}}),
    )
    .await;
    harness.advance(Duration::from_secs(31));
    harness.run_due_jobs().await;
    let edits: Vec<u64> = harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::PostMessage {
                message_id: Some(m),
                ..
            } => Some(m.get()),
            _ => None,
        })
        .collect();
    assert_eq!(edits, [77]);
    assert_eq!(harness.actions().len(), 1);
    assert_eq!(harness.jobs(), 0, "nothing is left to retry");
}

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(
        closure_i18n::missing_keys(crate::EN, crate::JA).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        closure_i18n::missing_keys(crate::JA, crate::EN).unwrap(),
        Vec::<String>::new()
    );
}
