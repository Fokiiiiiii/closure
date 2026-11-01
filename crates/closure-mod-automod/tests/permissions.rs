#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test setup")]

mod common;

use std::sync::Arc;
use std::time::Duration;

use closure_config::{ConfigCompiler, parse_overrides};
use closure_core::action::{Action, OverwriteTarget};
use closure_core::error::DiscordError;
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use common::*;
use serde_json::{Value, json};
use twilight_model::channel::Channel;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

fn setup(original: Option<Value>, dry_run: bool) -> closure_sdk::testing::Harness {
    let harness = configured(json!({
        "dry_run": dry_run,
        "rules": rules(&[("r", rule(json!([{ "type": "any_message" }]),
            json!([{ "do": "change_permissions", "deny": ["send_messages"], "duration": "60s" }])))])
    }));
    let channel: Channel = serde_json::from_value(json!({
        "id": CHANNEL.to_string(), "type": 0, "guild_id": GUILD.to_string(),
        "name": "test", "permission_overwrites": original.into_iter().collect::<Vec<_>>()
    }))
    .unwrap();
    harness.api().set_channels(Id::new(GUILD), vec![channel]);
    harness
}

fn overwrite() -> Value {
    json!({ "id": GUILD.to_string(), "type": 0, "allow": "64", "deny": "128" })
}

#[tokio::test]
async fn change_permissions_restores_original_and_extends_without_another_edit() {
    let harness = setup(Some(overwrite()), false);
    say(&harness, MEMBER, "one").await;
    let saved: Value = harness
        .store()
        .get("permissions:5:role:1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["original"]["allow"], json!("64"));
    assert_eq!(saved["original"]["deny"], json!("128"));
    harness.advance(Duration::from_secs(30));
    say(&harness, MEMBER, "two").await;
    let later: Value = harness
        .store()
        .get("permissions:5:role:1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["original"], later["original"]);
    assert_eq!(
        later["until"].as_i64().unwrap(),
        saved["until"].as_i64().unwrap() + 30
    );
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::SetPermissionOverwrite { .. }
        )),
        1
    );
    harness.advance(Duration::from_secs(30));
    assert!(harness.run_due_jobs().await.is_empty());
    harness.advance(Duration::from_secs(30));
    let results = harness.run_due_jobs().await;
    assert_eq!(results.len(), 1);
    assert!(results.iter().all(Result::is_ok));
    let edits = harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|a| match a {
            Action::SetPermissionOverwrite { overwrite, .. } => Some(overwrite),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(edits.len(), 2);
    assert!(edits[0].deny.contains(Permissions::SEND_MESSAGES));
    assert_eq!(edits[1].allow.bits(), 64);
    assert_eq!(edits[1].deny.bits(), 128);
    assert!(
        harness
            .store()
            .get::<Value>("permissions:5:role:1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn change_permissions_serializes_simultaneous_hits() {
    let harness = setup(Some(overwrite()), false);
    let first = harness.message(closure_sdk::testing::Message::new(MEMBER, CHANNEL).content("one"));
    let second =
        harness.message(closure_sdk::testing::Message::new(OTHER_MEMBER, CHANNEL).content("two"));
    let (first, second) = tokio::join!(first, second);
    assert!(first.is_ok() && second.is_ok());
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::SetPermissionOverwrite { .. }
        )),
        1
    );
    let saved: Value = harness
        .store()
        .get("permissions:5:role:1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["original"]["allow"], json!("64"));
}

#[tokio::test]
async fn change_permissions_removes_new_overwrite_on_expiry() {
    let harness = setup(None, false);
    say(&harness, MEMBER, "one").await;
    harness.advance(Duration::from_secs(60));
    let results = harness.run_due_jobs().await;
    assert_eq!(results.len(), 1);
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(
        count_actions(
            &harness,
            |a| matches!(a, Action::DeletePermissionOverwrite { target: OverwriteTarget::Role(id), .. } if id.get() == GUILD)
        ),
        1
    );
}

#[tokio::test]
async fn change_permissions_dry_run_and_missing_channel() {
    let dry = setup(None, true);
    say(&dry, MEMBER, "one").await;
    assert_eq!(
        count_actions(&dry, |a| matches!(a, Action::SetPermissionOverwrite { .. })),
        0
    );
    assert!(
        dry.store()
            .get::<Value>("permissions:5:role:1")
            .await
            .unwrap()
            .is_none()
    );
    assert!(dry.api().actions().iter().any(|a| matches!(a, Action::SendMessage { embeds, .. } if embeds.iter().any(|e| e.title.as_deref().is_some_and(|t| t.contains("dry run")) && e.fields.iter().any(|field| field.value.contains("change permissions"))))));
    let harness = setup(None, false);
    say(&harness, MEMBER, "one").await;
    harness.api().fail_nth(1, DiscordError::NotFound);
    harness.advance(Duration::from_secs(60));
    let results = harness.run_due_jobs().await;
    assert_eq!(results.len(), 1);
    assert!(results.iter().all(Result::is_ok));
    assert!(
        harness
            .store()
            .get::<Value>("permissions:5:role:1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn change_permissions_retries_failed_restoration() {
    let harness = setup(Some(overwrite()), false);
    say(&harness, MEMBER, "one").await;
    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("test".into()));
    harness.advance(Duration::from_secs(60));
    let failed = harness.run_due_jobs().await;
    assert_eq!(failed.len(), 1);
    assert!(failed[0].is_err());
    assert!(
        harness
            .store()
            .get::<Value>("permissions:5:role:1")
            .await
            .unwrap()
            .is_some()
    );
    harness.advance(Duration::from_secs(5));
    let retried = harness.run_due_jobs().await;
    assert_eq!(retried.len(), 1);
    assert!(retried[0].is_ok());
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::SetPermissionOverwrite { .. }
        )),
        2
    );
}

#[tokio::test]
async fn change_permissions_applies_allow_clear_and_member_target() {
    let harness = setup(Some(overwrite()), false);
    harness.set_config(json!({"rules": rules(&[("r", rule(json!([{"type":"any_message"}]),
        json!([{"do":"change_permissions", "allow":["send_messages"], "clear":["add_reactions"], "deny":["attach_files"], "duration":"60s"}])))] )}));
    say(&harness, MEMBER, "one").await;
    let edit = harness
        .api()
        .actions()
        .into_iter()
        .find_map(|a| match a {
            Action::SetPermissionOverwrite { overwrite, .. } => Some(overwrite),
            _ => None,
        })
        .unwrap();
    assert_eq!(edit.allow, Permissions::SEND_MESSAGES);
    assert_eq!(
        edit.deny,
        Permissions::from_bits_retain(128) | Permissions::ATTACH_FILES
    );

    let member_harness = setup(None, false);
    member_harness.set_config(json!({"rules": rules(&[("r", rule(json!([{"type":"any_message"}]),
        json!([{"do":"change_permissions", "target": MEMBER.to_string(), "deny":["send_messages"], "duration":"60s"}])))] )}));
    declare(&member_harness, MEMBER, &[]);
    say(&member_harness, MEMBER, "one").await;
    assert_eq!(
        count_actions(
            &member_harness,
            |a| matches!(a, Action::SetPermissionOverwrite { overwrite, .. } if overwrite.target == OverwriteTarget::Member(Id::new(MEMBER)))
        ),
        1
    );
    member_harness
        .store()
        .delete("permissions:5:member:7")
        .await
        .unwrap();
    member_harness.advance(Duration::from_secs(60));
    let results = member_harness.run_due_jobs().await;
    assert_eq!(results.len(), 1);
    assert!(results[0].is_ok());
    assert_eq!(
        count_actions(&member_harness, |a| matches!(
            a,
            Action::DeletePermissionOverwrite { .. }
        )),
        0
    );
}

fn role(id: u64, position: i64) -> twilight_model::guild::Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": "role", "color": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "hoist": false, "managed": false, "mentionable": false, "permissions": "0",
        "position": position, "flags": 0
    }))
    .unwrap()
}

#[tokio::test]
async fn change_permissions_enforces_role_hierarchy_and_handles_deleted_role() {
    for (target, allowed) in [(77, true), (78, false), (79, false)] {
        let harness = setup(None, false);
        harness.set_config(json!({"rules": rules(&[("r", rule(json!([{"type":"any_message"}]),
            json!([{"do":"change_permissions", "target": target.to_string(), "deny":["send_messages"], "duration":"60s"}])))] )}));
        harness
            .api()
            .set_roles(Id::new(GUILD), vec![role(77, 1), role(78, 2), role(79, 3)]);
        harness
            .api()
            .set_member(Id::new(GUILD), member(BOT, &[78], true, None));
        let result = harness
            .message(closure_sdk::testing::Message::new(MEMBER, CHANNEL).content("one"))
            .await;
        assert_eq!(result.is_ok(), allowed);
        assert_eq!(
            count_actions(&harness, |a| matches!(
                a,
                Action::SetPermissionOverwrite { .. }
            )),
            usize::from(allowed)
        );
        if allowed {
            harness
                .api()
                .set_roles(Id::new(GUILD), vec![role(78, 2), role(79, 3)]);
            harness.advance(Duration::from_secs(60));
            let results = harness.run_due_jobs().await;
            assert_eq!(results.len(), 1);
            assert!(results[0].is_ok());
            assert_eq!(
                count_actions(&harness, |a| matches!(
                    a,
                    Action::DeletePermissionOverwrite { .. }
                )),
                0
            );
            assert!(
                harness
                    .store()
                    .get::<Value>("permissions:5:role:77")
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[tokio::test]
async fn change_permissions_does_not_restore_a_role_that_moved_above_the_bot() {
    let harness = setup(None, false);
    harness.set_config(json!({"rules": rules(&[("r", rule(json!([{"type":"any_message"}]),
        json!([{"do":"change_permissions", "target":"77", "deny":["send_messages"], "duration":"60s"}])))] )}));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(77, 1), role(78, 2)]);
    harness
        .api()
        .set_member(Id::new(GUILD), member(BOT, &[78], true, None));
    say(&harness, MEMBER, "one").await;
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(77, 3), role(78, 2)]);
    harness.advance(Duration::from_secs(60));
    let results = harness.run_due_jobs().await;
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::DeletePermissionOverwrite { .. }
        )),
        0
    );
    assert!(
        harness
            .store()
            .get::<Value>("permissions:5:role:77")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn change_permissions_requires_a_readable_original_overwrite_list() {
    let harness = setup(None, false);
    let channel: Channel = serde_json::from_value(json!({
        "id": CHANNEL.to_string(), "type": 0, "guild_id": GUILD.to_string(), "name": "test"
    }))
    .unwrap();
    harness.api().set_channels(Id::new(GUILD), vec![channel]);
    let result = harness
        .message(closure_sdk::testing::Message::new(MEMBER, CHANNEL).content("one"))
        .await;
    assert!(result.is_err());
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::SetPermissionOverwrite { .. }
        )),
        0
    );
    assert!(
        harness
            .store()
            .get::<Value>("permissions:5:role:1")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn change_permissions_rejects_invalid_rules() {
    for (trigger, action) in [
        (
            json!({"type":"any_message"}),
            json!({"do":"change_permissions", "deny":["administrator"], "duration":"1m"}),
        ),
        (
            json!({"type":"any_message"}),
            json!({"do":"change_permissions", "deny":["send_messages"]}),
        ),
        (
            json!({"type":"member_join"}),
            json!({"do":"change_permissions", "deny":["send_messages"], "duration":"1m"}),
        ),
        (
            json!({"type":"any_message"}),
            json!({"do":"change_permissions", "deny":["send_messages"], "duration":"0s"}),
        ),
        (
            json!({"type":"any_message"}),
            json!({"do":"change_permissions", "deny":["send_messages"], "duration":"7h"}),
        ),
    ] {
        let harness = with_rules(rules(&[("r", rule(json!([trigger]), json!([action])))]));
        let replies = harness
            .invoke(
                closure_sdk::testing::Invocation::new("automod")
                    .subcommand("status")
                    .roles([MODERATOR_ROLE]),
            )
            .await
            .unwrap();
        assert!(replies[0].content.contains("1 invalid"), "{:?}", replies[0]);
    }
}

#[test]
fn change_permissions_schema_rejects_unlisted_permissions_and_missing_duration() {
    let registry = Arc::new(
        ModuleRegistry::new(vec![
            Arc::new(closure_mod_automod::module().unwrap()) as Arc<dyn Module>
        ])
        .unwrap(),
    );
    let compiler = ConfigCompiler::new(registry).unwrap();
    let accepted = |action: Value| {
        let config =
            json!({"rules": {"r": rule(json!([{"type":"any_message"}]), json!([action]))}});
        let yaml = format!("modules:\n  automod:\n    config: {config}\n");
        compiler.compile(&parse_overrides(&yaml).unwrap()).is_ok()
    };
    assert!(accepted(
        json!({"do":"change_permissions", "deny":["send_messages"], "duration":"1m"})
    ));
    assert!(!accepted(
        json!({"do":"change_permissions", "deny":["manage_roles"], "duration":"1m"})
    ));
    assert!(!accepted(
        json!({"do":"change_permissions", "deny":["send_messages"]})
    ));
}
