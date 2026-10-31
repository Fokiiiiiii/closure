//! Moderation cases, written with the SDK only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::time::Duration;

use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{Capability, CaseKind, Command, ModuleBuilder, ModuleId, NewCase};
use twilight_model::id::Id;

fn module() -> ModuleBuilder {
    ModuleBuilder::new("warn", "Warnings", "0.1.0")
        .unwrap()
        .capability(Capability::Cases)
        .command(Command::<()>::new("warn").handler(|cx, ()| async move {
            let case = cx
                .cases()
                .create(NewCase {
                    kind: CaseKind::new("warn")?,
                    target: cx.user().id,
                    moderator: cx.user().id,
                    reason: Some("spam".into()),
                    expires_at: None,
                })
                .await?;
            cx.reply(format!("Case {}.", case.number)).await
        }))
        .command(Command::<()>::new("revoke").handler(|cx, ()| async move {
            cx.cases().revoke(1, cx.user().id, None).await?;
            cx.reply("Revoked.").await
        }))
}

/// A module that did not declare the capability `cases`.
fn module_without_the_capability() -> ModuleBuilder {
    ModuleBuilder::new("plain", "Plain", "0.1.0")
        .unwrap()
        .command(Command::<()>::new("warn").handler(|cx, ()| async move {
            cx.cases()
                .create(NewCase {
                    kind: CaseKind::new("warn")?,
                    target: cx.user().id,
                    moderator: cx.user().id,
                    reason: None,
                    expires_at: None,
                })
                .await?;
            cx.reply("Recorded.").await
        }))
}

fn texts(replies: &[Reply]) -> Vec<&str> {
    replies.iter().map(|reply| reply.content.as_str()).collect()
}

#[tokio::test]
async fn a_command_records_cases_and_each_gets_the_next_number() {
    let harness = Harness::new(module()).unwrap();
    let first = harness
        .invoke(Invocation::new("warn").user(7))
        .await
        .unwrap();
    assert_eq!(texts(&first), ["Case 1."]);
    let second = harness
        .invoke(Invocation::new("warn").user(8))
        .await
        .unwrap();
    assert_eq!(texts(&second), ["Case 2."]);
}

#[tokio::test]
async fn a_recorded_case_reads_back_through_the_harness() {
    let harness = Harness::new(module()).unwrap();
    harness
        .invoke(Invocation::new("warn").user(7))
        .await
        .unwrap();
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.number, 1);
    assert_eq!(case.module, ModuleId::new("warn").unwrap());
    assert_eq!(case.kind, CaseKind::new("warn").unwrap());
    assert_eq!(case.target, Id::new(7));
    assert_eq!(case.moderator, Id::new(7));
    assert_eq!(case.reason.as_deref(), Some("spam"));
    assert_eq!(case.revoked, None);
}

#[tokio::test]
async fn a_case_is_revoked_once() {
    let harness = Harness::new(module()).unwrap();
    harness
        .invoke(Invocation::new("warn").user(7))
        .await
        .unwrap();
    let replies = harness
        .invoke(Invocation::new("revoke").user(9))
        .await
        .unwrap();
    assert_eq!(texts(&replies), ["Revoked."]);
    let revoked = harness
        .cases()
        .get(1)
        .await
        .unwrap()
        .unwrap()
        .revoked
        .unwrap();
    assert_eq!(revoked.by, Id::new(9));

    let again = harness.invoke(Invocation::new("revoke").user(9)).await;
    assert!(
        again
            .unwrap_err()
            .to_string()
            .contains("the case is already revoked")
    );
    assert_eq!(
        harness.cases().get(1).await.unwrap().unwrap().revoked,
        Some(revoked)
    );
}

#[tokio::test]
async fn a_module_without_the_capability_cannot_record_cases() {
    let harness = Harness::new(module_without_the_capability()).unwrap();
    let error = harness
        .invoke(Invocation::new("warn").user(7))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not granted the capability `cases`")
    );
}

#[tokio::test]
async fn cases_are_stamped_with_the_harness_clock() {
    let harness = Harness::new(module()).unwrap();
    harness
        .invoke(Invocation::new("warn").user(7))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(60));
    harness
        .invoke(Invocation::new("warn").user(7))
        .await
        .unwrap();
    let first = harness.cases().get(1).await.unwrap().unwrap().created_at;
    let second = harness.cases().get(2).await.unwrap().unwrap().created_at;
    assert_eq!(second - first, 60);
}
