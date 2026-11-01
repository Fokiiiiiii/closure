#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::action::Action;
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
    assert!(
        !h.api().actions()[before..]
            .iter()
            .any(|a| matches!(a, Action::EditChannel { .. }))
    );
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
