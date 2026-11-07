use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use closure_core::action::Action;
use closure_core::links::{LinkChecker, LinkVerdict};
use closure_sdk::testing::{Harness, Message};

use crate::builder;

/// Flags every text with `evil` in it, and counts how often it was asked.
struct Fake {
    asked: AtomicUsize,
    down: bool,
}

#[async_trait]
impl LinkChecker for Fake {
    async fn check(&self, text: &str) -> Result<LinkVerdict, String> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        if self.down {
            return Err("down".into());
        }
        Ok(if text.contains("evil") {
            LinkVerdict::Unsafe("PHISHING".into())
        } else {
            LinkVerdict::Safe
        })
    }
}

fn harness(down: bool) -> (Harness, Arc<Fake>) {
    let fake = Arc::new(Fake {
        asked: AtomicUsize::new(0),
        down,
    });
    let harness = Harness::new(builder(fake.clone()).unwrap()).unwrap();
    harness.api().add_channel(
        closure_core::event::ChannelId::new(5),
        Some(closure_core::event::GuildId::new(1)),
    );
    (harness, fake)
}

fn removed(harness: &Harness) -> bool {
    harness
        .actions()
        .iter()
        .any(|a| matches!(a, Action::DeleteMessage { .. }))
}

#[tokio::test]
async fn a_message_with_a_flagged_link_is_removed_and_its_author_banned() {
    let (harness, _) = harness(false);
    harness
        .message(Message::new(7, 5).content("free nitro https://evil.example/x"))
        .await
        .unwrap();
    assert!(removed(&harness));
    assert!(harness.actions().iter().any(|a| matches!(
        a,
        Action::Ban { user_id, delete_message_seconds: 3600, .. } if user_id.get() == 7
    )));
}

#[tokio::test]
async fn kick_can_be_chosen_instead_of_a_ban() {
    let (harness, _) = harness(false);
    harness.set_config(serde_json::json!({"action": "kick"}));
    harness
        .message(Message::new(7, 5).content("https://evil.example"))
        .await
        .unwrap();
    assert!(removed(&harness));
    assert!(
        harness
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Kick { user_id, .. } if user_id.get() == 7))
    );
    assert!(
        !harness
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Ban { .. }))
    );
}

#[tokio::test]
async fn safe_links_text_without_links_and_bots_are_left_alone() {
    let (harness, fake) = harness(false);
    for message in [
        Message::new(7, 5).content("see https://example.com"),
        Message::new(7, 5).content("no links evil here"),
        Message::new(8, 5).content("https://evil.example").bot(),
    ] {
        harness.message(message).await.unwrap();
    }
    assert!(!removed(&harness));
    assert_eq!(fake.asked.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn an_answer_is_remembered_and_a_silent_service_removes_nothing() {
    let (harness, fake) = harness(false);
    for _ in 0..3 {
        harness
            .message(Message::new(7, 5).content("https://evil.example"))
            .await
            .unwrap();
    }
    assert_eq!(fake.asked.load(Ordering::Relaxed), 1);
    let (harness, _) = self::harness(true);
    harness
        .message(Message::new(7, 5).content("https://evil.example"))
        .await
        .unwrap();
    assert!(harness.actions().is_empty());
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

#[tokio::test]
async fn a_link_far_into_a_long_message_is_checked_and_a_service_that_is_down_is_asked_once() {
    let (harness, fake) = harness(false);
    // 60 safe links, then a flagged one: it does not sit in the first question, but within four.
    let safe: Vec<String> = (0..60)
        .map(|n| format!("https://ok{n}.example/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"))
        .collect();
    let text = format!("{} https://evil.example/x", safe.join(" "));
    harness
        .message(Message::new(7, 5).content(text))
        .await
        .unwrap();
    assert!(
        fake.asked.load(Ordering::Relaxed) > 1,
        "several questions, none cut in a link"
    );
    let (harness, fake) = self::harness(true);
    for _ in 0..5 {
        harness
            .message(Message::new(7, 5).content("https://evil.example"))
            .await
            .unwrap();
    }
    assert_eq!(
        fake.asked.load(Ordering::Relaxed),
        1,
        "after a failure it is left alone for a while"
    );
    assert!(!removed(&harness));
}
