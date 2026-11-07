#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::action::{Action, MentionPolicy};
use closure_core::event::CommandOptionValue;
use closure_mod_moderation::builder;
use closure_sdk::testing::{Harness, Invocation};
use serde_json::json;
use std::time::Duration;
use twilight_model::id::Id;

fn harness(overwrite: bool) -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    let overwrites = if overwrite {
        vec![json!({"id":"1","type":0,"allow":"2048","deny":"8192"})]
    } else {
        vec![]
    };
    let channel = serde_json::from_value(json!({
        "id":"5", "guild_id":"1", "type":0, "name":"general",
        "rate_limit_per_user": 42, "permission_overwrites": overwrites
    }))
    .unwrap();
    harness.api().set_channels(Id::new(1), vec![channel]);
    let member = serde_json::from_value(json!({
        "user": {"id":"100", "username":"tester", "discriminator":"0", "avatar":null},
        "nick":"Mod @everyone", "roles":[], "joined_at":null, "deaf":false, "mute":false, "flags":0
    }))
    .unwrap();
    harness.api().set_member(Id::new(1), member);
    harness
}

fn invocation(name: &str) -> Invocation {
    Invocation::new(name).owner().channel(5)
}

#[tokio::test]
async fn slowmode_bounds_and_timed_restore() {
    let h = harness(false);
    h.invoke(invocation("slowmode").option("seconds", CommandOptionValue::Integer(0)))
        .await
        .unwrap();
    assert!(h.api().actions().iter().any(
        |a| matches!(a, Action::EditChannel { patch, .. } if patch.slowmode_seconds == Some(0))
    ));
    h.advance(Duration::from_secs(3));
    let before = h.api().actions().len();
    h.invoke(invocation("slowmode").option("seconds", CommandOptionValue::Integer(21601)))
        .await
        .unwrap();
    // Above Discord's longest, Discord keeps its longest and the bot keeps the rest.
    assert!(h.api().actions()[before..].iter().any(
        |a| matches!(a, Action::EditChannel { patch, .. } if patch.slowmode_seconds == Some(21600))
    ));
    h.advance(Duration::from_secs(3));
    h.invoke(
        invocation("slowmode")
            .option("seconds", CommandOptionValue::Integer(30))
            .option("duration", CommandOptionValue::String("10s".into())),
    )
    .await
    .unwrap();
    h.advance(Duration::from_secs(10));
    let jobs = h.run_due_jobs().await;
    assert_eq!(jobs.len(), 1);
    assert!(jobs.into_iter().all(|r| r.is_ok()));
    assert!(h.api().actions().iter().any(
        |a| matches!(a, Action::EditChannel { patch, .. } if patch.slowmode_seconds == Some(42))
    ));
}

#[tokio::test]
async fn lock_twice_unlock_and_timed_restore() {
    let h = harness(true);
    h.invoke(invocation("lock")).await.unwrap();
    assert!(h.api().actions().iter().any(|a| matches!(
        a, Action::SetPermissionOverwrite { overwrite, .. }
            if overwrite.deny.contains(
                twilight_model::guild::Permissions::SEND_MESSAGES
                | twilight_model::guild::Permissions::SEND_MESSAGES_IN_THREADS
                | twilight_model::guild::Permissions::ADD_REACTIONS
            )
    )));
    let locked = h
        .api()
        .actions()
        .iter()
        .find_map(|a| match a {
            Action::SetPermissionOverwrite { overwrite, .. } => Some(overwrite.clone()),
            _ => None,
        })
        .unwrap();
    let channel = serde_json::from_value(json!({
        "id":"5", "guild_id":"1", "type":0, "name":"general",
        "rate_limit_per_user":42,
        "permission_overwrites":[{"id":"1", "type":0, "allow":locked.allow.bits().to_string(), "deny":locked.deny.bits().to_string()}]
    })).unwrap();
    h.api().set_channels(Id::new(1), vec![channel]);
    h.advance(Duration::from_secs(3));
    h.invoke(invocation("lock")).await.unwrap();
    h.invoke(invocation("unlock")).await.unwrap();
    let actions = h.api().actions();
    let last = actions
        .iter()
        .rev()
        .find_map(|a| {
            if let Action::SetPermissionOverwrite { overwrite, .. } = a {
                Some(overwrite)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(last.allow.bits(), 2048);
    assert_eq!(last.deny.bits(), 8192);
    let h = harness(true);
    h.advance(Duration::from_secs(3));
    h.invoke(invocation("lock").option("duration", CommandOptionValue::String("10s".into())))
        .await
        .unwrap();
    h.advance(Duration::from_secs(10));
    let jobs = h.run_due_jobs().await;
    assert_eq!(jobs.len(), 1);
    assert!(jobs.into_iter().all(|r| r.is_ok()));
    let actions = h.api().actions();
    let last = actions
        .iter()
        .rev()
        .find_map(|a| {
            if let Action::SetPermissionOverwrite { overwrite, .. } = a {
                Some(overwrite)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(last.allow.bits(), 2048);
    assert_eq!(last.deny.bits(), 8192);
}

#[tokio::test]
async fn absent_overwrite_prefix_and_permission() {
    let h = harness(false);
    h.invoke(Invocation::new("lock").channel(5)).await.unwrap();
    h.invoke(
        Invocation::new("slowmode")
            .channel(5)
            .option("seconds", CommandOptionValue::Integer(5)),
    )
    .await
    .unwrap();
    h.invoke(Invocation::new("unlock").channel(5))
        .await
        .unwrap();
    assert!(!h.api().actions().iter().any(|a| matches!(
        a,
        Action::SetPermissionOverwrite { .. }
            | Action::EditChannel { .. }
            | Action::DeletePermissionOverwrite { .. }
    )));
    h.invoke(invocation("lock").text_command()).await.unwrap();
    assert!(
        h.api()
            .actions()
            .iter()
            .any(|a| matches!(a, Action::SetPermissionOverwrite { .. }))
    );
    h.invoke(invocation("unlock").text_command()).await.unwrap();
    assert!(
        h.api()
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DeletePermissionOverwrite { .. }))
    );
}

#[tokio::test]
async fn timed_lock_deletes_an_overwrite_that_did_not_exist() {
    let h = harness(false);
    h.invoke(invocation("lock").option("duration", CommandOptionValue::String("10s".into())))
        .await
        .unwrap();
    h.advance(Duration::from_secs(10));
    let jobs = h.run_due_jobs().await;
    assert_eq!(jobs.len(), 1);
    assert!(jobs.into_iter().all(|r| r.is_ok()));
    assert!(
        h.api()
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DeletePermissionOverwrite { .. }))
    );
}

#[tokio::test]
async fn channel_audit_and_log() {
    let h = harness(false);
    h.set_log_channel(77);
    h.invoke(
        invocation("slowmode")
            .option("seconds", CommandOptionValue::Integer(30))
            .option("duration", CommandOptionValue::String("10s".into())),
    )
    .await
    .unwrap();
    h.invoke(
        invocation("lock")
            .option(
                "reason",
                CommandOptionValue::String("@everyone <@123>".into()),
            )
            .option("duration", CommandOptionValue::String("10s".into())),
    )
    .await
    .unwrap();
    h.invoke(invocation("unlock")).await.unwrap();
    let actions = h.api().actions();
    let reasons: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::EditChannel { reason, .. }
            | Action::SetPermissionOverwrite { reason, .. }
            | Action::DeletePermissionOverwrite { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 3);
    assert!(
        reasons
            .iter()
            .all(|reason| reason.contains("100") && reason.contains("Mod @everyone"))
    );
    assert!(reasons[1].contains("@everyone <@123>"));
    let logs: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                channel_id,
                content: Some(content),
                mentions,
                ..
            } if channel_id.get() == 77 => Some((content, mentions)),
            _ => None,
        })
        .collect();
    assert_eq!(logs.len(), 3);
    assert!(
        logs.iter()
            .all(|(content, mentions)| content.contains("Mod @")
                && content.contains("<#5>")
                && !content.contains('\n')
                && matches!(mentions, MentionPolicy::None))
    );
    assert!(logs[0].0.contains("30") && logs[0].0.contains("<t:"));
    assert!(logs[1].0.contains("Locked") && logs[1].0.contains("<t:"));
    assert!(logs[2].0.contains("Unlocked"));
    h.advance(Duration::from_secs(10));
    assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
    assert!(h.api().actions().iter().any(|a| matches!(a, Action::SendMessage { content: Some(content), mentions: MentionPolicy::None, .. } if content.contains("System") && content.contains("42"))));
}

#[tokio::test]
async fn automatic_unlock_is_logged() {
    let h = harness(false);
    h.set_log_channel(77);
    h.invoke(invocation("lock").option("duration", CommandOptionValue::String("10s".into())))
        .await
        .unwrap();
    h.advance(Duration::from_secs(10));
    assert!(h.run_due_jobs().await.into_iter().all(|r| r.is_ok()));
    assert!(h.api().actions().iter().any(|a| matches!(a, Action::SendMessage { content: Some(content), mentions: MentionPolicy::None, .. } if content.contains("System") && content.contains("Unlocked"))));
}

#[tokio::test]
async fn channel_commands_succeed_without_log_or_when_post_fails() {
    for name in ["slowmode", "lock", "unlock"] {
        for fail in [false, true] {
            let h = harness(false);
            if fail {
                h.set_log_channel(77);
            }
            if name == "unlock" {
                h.invoke(invocation("lock")).await.unwrap();
            }
            let make_invocation = || {
                let command = invocation(name);
                if name == "slowmode" {
                    command.option("seconds", CommandOptionValue::Integer(10))
                } else {
                    command
                }
            };
            if fail {
                // Defer, channel lookup when needed, change, then log.
                let nth = if name == "unlock" { 4 } else { 5 };
                h.api()
                    .fail_nth(nth, closure_sdk::DiscordError::Fatal("offline".into()));
            }
            let before = h.api().actions().len();
            let replies = h.invoke(make_invocation()).await.unwrap();
            assert!(
                replies.last().unwrap().content.contains(match name {
                    "slowmode" => "Slowmode",
                    "lock" => "Locked",
                    _ => "Restored",
                }),
                "{name} {fail}: {replies:?}"
            );
            assert!(!h.api().actions()[before..].iter().any(
                |a| matches!(a, Action::SendMessage { channel_id, .. } if channel_id.get() == 77)
            ));
        }
    }
}

#[tokio::test]
async fn audit_reason_preserves_actor_with_long_reason() {
    let h = harness(false);
    h.invoke(invocation("lock").option("reason", CommandOptionValue::String("界".repeat(512))))
        .await
        .unwrap();
    let actions = h.api().actions();
    let reason = actions
        .iter()
        .find_map(|a| match a {
            Action::SetPermissionOverwrite { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(reason.contains("100") && reason.contains("Mod @everyone"));
    assert_eq!(reason.chars().count(), 512);
    assert!(reason.ends_with('…'));
}
