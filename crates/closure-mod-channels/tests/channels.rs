#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::action::{Action, Emoji};
use closure_sdk::{
    DiscordError,
    testing::{Harness, Message},
};
use serde_json::json;
use std::time::Duration;
use twilight_model::id::Id;

fn harness(config: serde_json::Value) -> Harness {
    let h = Harness::new(closure_mod_channels::builder().unwrap()).unwrap();
    h.api().add_channel(Id::new(5), Some(Id::new(1)));
    h.api().add_channel(Id::new(6), Some(Id::new(1)));
    h.set_config(config);
    h
}

fn delete_config() -> serde_json::Value {
    json!({"auto_delete": [{"channel": "5", "after": "5s"}]})
}

#[tokio::test]
async fn unresolved_log_webhook_blocks_deletion() {
    let h = harness(delete_config());
    h.set_log_webhook("missing");
    h.message(Message::new(7, 5)).await.unwrap();
    assert_eq!(h.jobs(), 0);
}

#[tokio::test]
async fn log_destinations_are_protected_before_and_after_scheduling() {
    for destination in ["channel", "fallback", "webhook", "logs_webhook"] {
        for change_after_schedule in [false, true] {
            let h = harness(delete_config());
            if change_after_schedule {
                h.message(Message::new(7, 5)).await.unwrap();
                assert_eq!(h.jobs(), 1);
            }
            match destination {
                "channel" => h.set_logs_config(json!({"channel":"5"})),
                "fallback" => {
                    h.set_log_channel(5);
                    h.set_log_webhook("audit");
                    h.api().add_named_webhook(Id::new(1), "audit", Id::new(6));
                }
                "webhook" => {
                    h.set_log_webhook("audit");
                    h.api().add_named_webhook(Id::new(1), "audit", Id::new(5));
                }
                _ => {
                    h.set_logs_config(json!({"webhook":"audit"}));
                    h.api().add_named_webhook(Id::new(1), "audit", Id::new(5));
                }
            }
            if change_after_schedule {
                for _ in 0..3 {
                    h.advance(Duration::from_secs(10));
                    assert!(
                        h.run_due_jobs().await.iter().all(Result::is_ok),
                        "{destination}"
                    );
                }
                assert!(
                    !h.actions()
                        .iter()
                        .any(|a| matches!(a, Action::DeleteMessage { .. })),
                    "{destination}"
                );
            } else {
                h.message(Message::new(7, 5)).await.unwrap();
                assert_eq!(h.jobs(), 0, "{destination}");
            }
        }
    }
}

#[tokio::test]
async fn unresolved_webhook_blocks_already_scheduled_deletion() {
    let h = harness(delete_config());
    h.message(Message::new(7, 5)).await.unwrap();
    h.set_log_webhook("missing");
    for _ in 0..3 {
        h.advance(Duration::from_secs(10));
        assert!(h.run_due_jobs().await.iter().all(Result::is_ok));
    }
    assert!(h.actions().is_empty());
}

#[tokio::test]
async fn schedules_and_deletes_after_delay() {
    let h = harness(delete_config());
    h.message(Message::new(7, 5)).await.unwrap();
    // One job sweeps the guild, whatever the number of messages.
    h.message(Message::new(8, 5)).await.unwrap();
    assert_eq!(h.jobs(), 1);
    let deleted = |h: &Harness| {
        h.actions()
            .iter()
            .filter(|a| matches!(a, Action::DeleteMessage { .. }))
            .count()
    };
    assert!(h.run_due_jobs().await.iter().all(Result::is_ok));
    h.advance(Duration::from_secs(4));
    assert!(h.run_due_jobs().await.iter().all(Result::is_ok));
    assert_eq!(deleted(&h), 0, "not before the delay");
    // The sets are ten seconds wide and the sweep runs every ten, so a message goes within a few turns.
    for _ in 0..3 {
        h.advance(Duration::from_secs(10));
        assert!(h.run_due_jobs().await.iter().all(Result::is_ok));
    }
    assert_eq!(deleted(&h), 2);
}

#[tokio::test]
async fn a_first_sweep_that_comes_late_still_deletes_what_fell_due_before_it() {
    let h = harness(delete_config());
    h.message(Message::new(7, 5)).await.unwrap();
    // The sweep job is not run until a minute has passed: the message fell due long before.
    h.advance(Duration::from_secs(60));
    for _ in 0..4 {
        assert!(h.run_due_jobs().await.iter().all(Result::is_ok));
        h.advance(Duration::from_secs(10));
    }
    let deleted = h
        .actions()
        .iter()
        .filter(|a| matches!(a, Action::DeleteMessage { .. }))
        .count();
    assert_eq!(deleted, 1);
}

#[tokio::test]
async fn missing_and_log_channel_are_protected() {
    let h = harness(delete_config());
    h.message(Message::new(7, 5)).await.unwrap();
    h.api().fail_nth(1, DiscordError::NotFound);
    for _ in 0..3 {
        h.advance(Duration::from_secs(10));
        assert!(h.run_due_jobs().await.iter().all(Result::is_ok));
    }
    assert!(
        !h.actions()
            .iter()
            .any(|a| matches!(a, Action::DeleteMessage { .. }))
    );
    let h = harness(delete_config());
    h.message(Message::new(7, 5)).await.unwrap();
    h.set_log_channel(5);
    for _ in 0..3 {
        h.advance(Duration::from_secs(10));
        assert!(h.run_due_jobs().await.iter().all(Result::is_ok));
    }
    assert!(h.actions().is_empty());
    h.message(Message::new(7, 5)).await.unwrap();
    assert_eq!(
        h.jobs(),
        1,
        "only the sweep, nothing is kept for the log channel"
    );
}

#[tokio::test]
async fn reactions_do_not_touch_log_channel() {
    let h = harness(json!({"auto_react":[{"channel":"5","emojis":["👍"],"skip_bots":false}]}));
    h.set_log_channel(5);
    h.message(Message::new(7, 5)).await.unwrap();
    assert!(h.actions().is_empty());
}

#[tokio::test]
async fn reactions_do_not_touch_logs_override_or_unresolved_webhook() {
    for unresolved in [false, true] {
        let h = harness(json!({"auto_react":[{"channel":"5","emojis":["👍"],"skip_bots":false}]}));
        if unresolved {
            h.set_log_webhook("missing");
        } else {
            h.set_logs_config(json!({"channel":"5"}));
        }
        h.message(Message::new(7, 5)).await.unwrap();
        assert!(h.actions().is_empty());
    }
}

#[tokio::test]
async fn reactions_are_ordered_skip_bots_and_continue_after_failure() {
    let h = harness(json!({"auto_react":[{"channel":"5","emojis":["👍","❤️","🎉"]}]}));
    h.api().fail_nth(2, DiscordError::Forbidden("test".into()));
    h.message(Message::new(7, 5)).await.unwrap();
    let emojis: Vec<_> = h
        .actions()
        .into_iter()
        .filter_map(|a| match a {
            Action::AddReaction {
                emoji: Emoji::Unicode { value },
                ..
            } => Some(value),
            _ => None,
        })
        .collect();
    assert_eq!(emojis, ["👍", "🎉"]);
    h.message(Message::new(7, 5).bot()).await.unwrap();
    h.message(Message::new(7, 6)).await.unwrap();
    assert_eq!(h.actions().len(), 2);
}

#[tokio::test]
async fn invalid_limits_and_duplicate_first_rule() {
    for after in ["4s", "8d", "bad"] {
        let h = harness(json!({"auto_delete":[{"channel":"5","after":after}]}));
        assert!(h.message(Message::new(7, 5)).await.is_err());
    }
    let h = harness(json!({"auto_react":[{"channel":"5","emojis":[]}]}));
    assert!(h.message(Message::new(7, 5)).await.is_err());
    let h = harness(json!({"auto_delete": vec![json!({"channel":"5","after":"5s"}); 26]}));
    assert!(h.message(Message::new(7, 5)).await.is_err());
    let h =
        harness(json!({"auto_delete":[{"channel":"5","after":"5s"},{"channel":"5","after":"1h"}]}));
    h.message(Message::new(7, 5)).await.unwrap();
    h.advance(Duration::from_secs(10));
    assert_eq!(h.run_due_jobs().await.len(), 1);
}
