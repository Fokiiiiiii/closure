//! Each action: the Discord calls it makes, the case it leaves, and the text it sends.
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

use closure_core::action::{Action, MentionPolicy};
use closure_core::error::DiscordError;
use closure_sdk::Event;
use closure_sdk::testing::Message;
use common::*;
use serde_json::{Value, json};
use twilight_model::id::Id;

fn words(word: &str) -> Value {
    json!({ "type": "words", "words": [word] })
}

fn spoiler_rule(actions: Value) -> Value {
    rules(&[("spoilers", rule(json!([words("spoiler")]), actions))])
}

// ---- delete ----

#[tokio::test]
async fn delete_removes_the_message_with_a_single_delete_and_records_an_automod_case() {
    let harness = with_rules(spoiler_rule(delete()));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::DeleteMessage { .. })),
        1
    );

    let reason = harness
        .api()
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::DeleteMessage { reason, .. } => Some(format!("{reason:?}")),
            _ => None,
        })
        .expect("the message is deleted");
    assert!(
        reason.contains("spoilers"),
        "the audit reason names the rule: {reason}"
    );

    let cases = cases_of(&harness, MEMBER).await;
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].kind.as_str(), "automod");
    assert_eq!(cases[0].moderator, Id::new(BOT));
    assert_eq!(cases[0].expires_at, None);
    assert!(
        harness.api().actions().iter().any(
            |a| matches!(a, Action::SendMessage { channel_id, .. } if *channel_id == Id::new(LOG))
        ),
        "the case is posted to the log"
    );
}

#[tokio::test]
async fn a_burst_is_deleted_in_one_bulk_request_when_it_is_in_one_channel() {
    let harness = with_rules(rules(&[(
        "flood",
        rule(
            json!([{ "type": "spam", "measure": "messages", "amount": 3, "within": "10s" }]),
            delete(),
        ),
    )]));
    for text in ["a", "b", "c"] {
        say(&harness, MEMBER, text).await;
    }
    assert_eq!(
        count_actions(
            &harness,
            |a| matches!(a, Action::BulkDeleteMessages { message_ids, .. } if message_ids.len() == 3)
        ),
        1
    );
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::DeleteMessage { .. })),
        0
    );
}

#[tokio::test]
async fn a_message_that_is_already_deleted_is_not_an_error() {
    let harness = with_rules(spoiler_rule(delete()));
    harness.api().fail_nth(1, DiscordError::NotFound);
    harness
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await
        .unwrap();
    assert_eq!(
        cases_of(&harness, MEMBER).await.len(),
        1,
        "the rule still left its case"
    );
}

#[tokio::test]
async fn a_failing_action_does_not_stop_the_next_one_and_the_error_is_reported() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "delete" },
        { "do": "alert", "channel": NOTIFY.to_string() }
    ])));
    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("no permission".into()));
    let result = harness
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await;
    assert!(result.is_err(), "the first error reaches the dispatcher");
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::SendMessage { .. })),
        2,
        "the alert ran (one message), and the case went to the log (one embed)"
    );
}

#[tokio::test]
async fn the_actions_run_in_the_order_given() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "alert", "channel": NOTIFY.to_string(), "text": "first" },
        { "do": "delete" },
        { "do": "alert", "channel": NOTIFY.to_string(), "text": "last" }
    ])));
    say(&harness, MEMBER, "spoiler").await;
    let order: Vec<&'static str> = harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                content, embeds, ..
            } => {
                let text = content
                    .clone()
                    .or_else(|| embeds.iter().find_map(|embed| embed.description.clone()));
                matches!(text.as_deref(), Some("first" | "last")).then_some("alert")
            }
            Action::DeleteMessage { .. } => Some("delete"),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["alert", "delete", "alert"]);
}

// ---- purge ----

#[tokio::test]
async fn purge_deletes_what_the_member_sent_within_the_window_from_the_history() {
    let harness = with_rules(rules(&[(
        "purge",
        rule(
            json!([words("bad")]),
            json!([{ "do": "purge", "within": "1h" }]),
        ),
    )]));
    seed(&harness, 9001, MEMBER, CHANNEL, 60).await;
    seed(&harness, 9002, MEMBER, OTHER_CHANNEL, 120).await;
    seed(&harness, 9003, MEMBER, CHANNEL, 7_200).await;
    seed(&harness, 9004, OTHER_MEMBER, CHANNEL, 60).await;
    say(&harness, MEMBER, "bad").await;

    let ids = deleted_ids(&harness);
    assert!(ids.contains(&9001) && ids.contains(&9002), "{ids:?}");
    assert!(!ids.contains(&9003), "older than the window: {ids:?}");
    assert!(!ids.contains(&9004), "another member's: {ids:?}");
}

// ---- punishments ----

#[tokio::test]
async fn warn_records_a_warning_that_expires_in_the_configured_days() {
    let harness = configured(json!({
        "warn_expire_days": 7,
        "rules": rules(&[("w", rule(json!([words("spoiler")]), json!([{ "do": "warn" }])))])
    }));
    say(&harness, MEMBER, "spoiler").await;
    let cases = cases_of(&harness, MEMBER).await;
    assert_eq!(cases.len(), 1, "a warning and no separate automod case");
    assert_eq!(cases[0].kind.as_str(), "warn");
    assert_eq!(cases[0].moderator, Id::new(BOT));
    assert_eq!(cases[0].expires_at, Some(cases[0].created_at + 7 * 86_400));
}

#[tokio::test]
async fn a_timeout_by_discord_sets_the_timeout_and_records_a_timeout_case() {
    let harness = with_rules(spoiler_rule(json!([{ "do": "timeout", "duration": "1h" }])));
    say(&harness, MEMBER, "spoiler").await;
    assert!(harness.api().actions().iter().any(|a| matches!(
        a,
        Action::Timeout { user_id, duration_seconds: Some(3600), .. } if *user_id == Id::new(MEMBER)
    )));
    let cases = cases_of(&harness, MEMBER).await;
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].kind.as_str(), "timeout");
}

#[tokio::test]
async fn a_timeout_by_role_adds_the_role_instead() {
    let harness = configured(json!({
        "timeout_method": "role",
        "timeout_role": "77",
        "rules": spoiler_rule(json!([{ "do": "timeout", "duration": "1h" }]))
    }));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![guild_role(ROLE, "Mods", 1, false)]);
    say(&harness, MEMBER, "spoiler").await;
    assert!(harness.api().actions().iter().any(|a| matches!(
        a,
        Action::AddRole { role_id, .. } if *role_id == Id::new(ROLE)
    )));
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::Timeout { .. })),
        0
    );
}

/// A role of the guild, as the pretend Discord lists it.
fn guild_role(id: u64, name: &str, position: i64, managed: bool) -> twilight_model::guild::Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": name, "color": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "hoist": false, "managed": managed, "mentionable": false, "permissions": "0",
        "position": position, "flags": 0
    }))
    .unwrap()
}

fn timeout_by_role(extra: Value) -> Value {
    let mut config = json!({
        "timeout_method": "role",
        "rules": spoiler_rule(json!([{ "do": "timeout", "duration": "1h" }]))
    });
    config
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    config
}

#[tokio::test]
async fn a_timeout_by_role_creates_the_role_when_none_is_configured() {
    let harness = configured(timeout_by_role(json!({})));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::CreateRole { .. })),
        1
    );
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::AddRole { .. })),
        1
    );
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::Timeout { .. })),
        0
    );
}

#[tokio::test]
async fn a_timeout_role_named_like_moderations_is_shared() {
    let harness = configured(timeout_by_role(json!({})));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![guild_role(700, "Timed out", 1, false)]);
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::CreateRole { .. })),
        0
    );
    assert!(harness.api().actions().iter().any(|a| matches!(
        a,
        Action::AddRole { role_id, .. } if *role_id == Id::new(700)
    )));
}

#[tokio::test]
async fn a_configuration_change_creates_the_timeout_role_before_a_punishment() {
    let harness = configured(json!({ "timeout_method": "role" }));
    harness.config_changed().await.unwrap();
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::CreateRole { .. })),
        1
    );
}

#[tokio::test]
async fn a_dry_run_configuration_change_creates_no_timeout_role() {
    let harness = configured(json!({ "timeout_method": "role", "dry_run": true }));
    harness.config_changed().await.unwrap();
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::CreateRole { .. })),
        0
    );
}

#[tokio::test]
async fn a_channel_created_later_gets_the_timeout_role_overwrite() {
    let harness = configured(json!({ "timeout_method": "role" }));
    harness.config_changed().await.unwrap();
    harness.api().add_channel(Id::new(88), Some(Id::new(GUILD)));
    harness.channel_created(88).await.unwrap();
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::SetPermissionOverwrite { channel_id, .. } if *channel_id == Id::new(88)
        )),
        1
    );
}

#[tokio::test]
async fn a_timeout_by_role_takes_roles_off_and_gives_them_back_when_its_timer_is_due() {
    let harness = configured(timeout_by_role(json!({ "remove_roles_on_mute": true })));
    harness.api().set_roles(
        Id::new(GUILD),
        vec![
            guild_role(GUILD, "everyone", 0, false),
            guild_role(60, "plain", 2, false),
            guild_role(61, "managed", 3, true),
        ],
    );
    declare(&harness, MEMBER, &[GUILD, 60, 61]);
    say(&harness, MEMBER, "spoiler").await;
    let removed = count_actions(&harness, |a| {
        matches!(
            a,
            Action::RemoveRole { role_id, .. } if *role_id == Id::new(60)
        )
    });
    assert_eq!(
        removed, 1,
        "the plain role goes; @everyone and the managed role stay"
    );
    harness.advance(Duration::from_secs(3_601));
    harness.run_due_jobs().await;
    assert!(harness.api().actions().iter().any(|a| matches!(
        a,
        Action::AddRole { role_id, .. } if *role_id == Id::new(60)
    )));
}

#[test]
fn the_role_settings_of_automod_take_false_true_or_role_ids() {
    let registry = std::sync::Arc::new(
        closure_core::registry::ModuleRegistry::new(vec![std::sync::Arc::new(
            closure_mod_automod::module().unwrap(),
        )
            as std::sync::Arc<dyn closure_core::module::Module>])
        .unwrap(),
    );
    let compiler = closure_config::ConfigCompiler::new(registry).unwrap();
    let accepted = |config: Value| {
        let yaml = format!(
            "modules:\n  automod:\n    config: {}\n",
            serde_json::to_string(&config).unwrap()
        );
        compiler
            .compile(&closure_config::parse_overrides(&yaml).unwrap())
            .is_ok()
    };
    assert!(accepted(json!({ "remove_roles_on_mute": true })));
    assert!(accepted(json!({ "restore_roles_on_mute": ["60"] })));
    assert!(!accepted(json!({ "remove_roles_on_mute": "all" })));
    assert!(!accepted(json!({ "restore_roles_on_mute": ["role"] })));
}

#[test]
fn a_regex_that_does_not_compile_is_refused_at_the_path_of_its_rule() {
    let registry = std::sync::Arc::new(
        closure_core::registry::ModuleRegistry::new(vec![std::sync::Arc::new(
            closure_mod_automod::module().unwrap(),
        )
            as std::sync::Arc<dyn closure_core::module::Module>])
        .unwrap(),
    );
    let compiler = closure_config::ConfigCompiler::new(registry).unwrap();
    // The problems of a document: none when it is accepted.
    let problems = |trigger: Value| {
        let config = json!({
            "rules": { "spoiler": { "triggers": [trigger], "actions": [{ "do": "delete" }] } }
        });
        let yaml = format!(
            "modules:\n  automod:\n    config: {}\n",
            serde_json::to_string(&config).unwrap()
        );
        match compiler.compile(&closure_config::parse_overrides(&yaml).unwrap()) {
            Ok(_) => Vec::new(),
            Err(error) => error.issues().to_vec(),
        }
    };
    let at_rule = "modules.automod.config.rules.spoiler";
    assert!(problems(json!({ "type": "regex", "patterns": ["spo+iler"] })).is_empty());
    assert!(problems(json!({ "type": "links", "include_regex": ["^https://"] })).is_empty());

    let broken = problems(json!({ "type": "regex", "patterns": ["("] }));
    assert!(
        broken.iter().any(|issue| issue.path.starts_with(at_rule)),
        "{broken:?}"
    );
    let broken_link = problems(json!({ "type": "links", "include_regex": ["("] }));
    assert!(
        broken_link
            .iter()
            .any(|issue| issue.path.starts_with(at_rule)),
        "{broken_link:?}"
    );
}

#[tokio::test]
async fn kick_kicks_the_member_and_records_a_kick_case() {
    let harness = with_rules(spoiler_rule(json!([{ "do": "kick" }])));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::Kick { .. })),
        1
    );
    assert_eq!(cases_of(&harness, MEMBER).await[0].kind.as_str(), "kick");
}

#[tokio::test]
async fn a_timed_ban_deletes_the_days_given_sets_a_timer_and_lifts_the_ban_when_due() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "ban", "duration": "1d", "delete_days": 2 }
    ])));
    say(&harness, MEMBER, "spoiler").await;
    assert!(harness.api().actions().iter().any(|a| matches!(
        a,
        Action::Ban {
            delete_message_seconds: 172_800,
            ..
        }
    )));
    assert_eq!(cases_of(&harness, MEMBER).await[0].kind.as_str(), "ban");
    assert_eq!(harness.jobs(), 1, "the timer of the ban");

    harness.advance(Duration::from_secs(86_401));
    let ended = harness.run_due_jobs().await;
    assert!(ended.iter().all(Result::is_ok));
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::Unban { .. })),
        1
    );
}

#[tokio::test]
async fn a_permanent_ban_sets_no_timer() {
    let harness = with_rules(spoiler_rule(json!([{ "do": "ban" }])));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::Ban { .. })),
        1
    );
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_punishment_the_discord_refuses_leaves_no_case() {
    let harness = with_rules(spoiler_rule(json!([{ "do": "kick" }])));
    harness
        .api()
        .fail_with(DiscordError::Forbidden("role too high".into()));
    let result = harness
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await;
    assert!(result.is_err());
    assert!(
        cases_of(&harness, MEMBER).await.is_empty(),
        "no case for an action that did not happen"
    );
}

// ---- reply ----

#[tokio::test]
async fn reply_posts_the_text_and_mentions_nobody() {
    let harness = with_rules(spoiler_rule(
        json!([{ "do": "reply", "text": "Keep it civil, {user}" }]),
    ));
    say(&harness, MEMBER, "spoiler").await;
    let sends: Vec<(Option<String>, MentionPolicy)> = harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                content,
                embeds,
                mentions,
                ..
            } => {
                let text =
                    content.or_else(|| embeds.into_iter().find_map(|embed| embed.description))?;
                Some((Some(text), mentions))
            }
            _ => None,
        })
        .collect();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].0.as_deref(), Some("Keep it civil, <@7>"));
    assert_eq!(sends[0].1, MentionPolicy::None);
}

#[tokio::test]
async fn reply_with_auto_delete_removes_the_reply_when_its_time_is_up() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "reply", "text": "noted", "auto_delete": "30s" }
    ])));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 0);

    harness.advance(Duration::from_secs(29));
    harness.run_due_jobs().await;
    assert_eq!(deletions(&harness), 0, "not yet");

    harness.advance(Duration::from_secs(2));
    let ended = harness.run_due_jobs().await;
    assert!(ended.iter().all(Result::is_ok));
    assert_eq!(deletions(&harness), 1, "the reply is removed");
}

// ---- alert ----

#[tokio::test]
async fn alert_posts_its_text_to_the_channel_it_names() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "alert", "channel": NOTIFY.to_string(), "text": "{rule} by {username} in {channel}" }
    ])));
    say(&harness, MEMBER, "spoiler").await;
    let texts = sent_texts(&harness);
    assert!(
        texts.contains(&(NOTIFY, "spoilers by tester in <#5>".to_owned())),
        "{texts:?}"
    );
}

#[tokio::test]
async fn alert_without_text_says_who_triggered_which_rule_and_what_was_found() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "alert", "channel": NOTIFY.to_string() }
    ])));
    say(&harness, MEMBER, "spoiler").await;
    let texts: Vec<String> = sent_texts(&harness)
        .into_iter()
        .filter(|(channel, _)| *channel == NOTIFY)
        .map(|(_, text)| text)
        .collect();
    assert_eq!(texts.len(), 1, "one alert");
    assert!(texts[0].contains("<@7>"), "{}", texts[0]);
    assert!(texts[0].contains("spoilers"), "{}", texts[0]);
    assert!(texts[0].contains("spoiler"), "{}", texts[0]);
}

// ---- templates ----

#[tokio::test]
async fn templates_fill_in_the_member_channel_rule_detail_link_guild_and_count() {
    let harness = with_rules(rules(&[(
        "tpl",
        json!({
            "name": "Templates",
            "triggers": [words("bad")],
            "actions": [{
                "do": "reply",
                "text": "{user}|{user_id}|{username}|{channel}|{rule}|{detail}|{message_link}|{guild}|{count}"
            }]
        }),
    )]));
    say(&harness, MEMBER, "bad").await;
    let texts = sent_texts(&harness);
    let text = &texts[0].1;
    let parts: Vec<&str> = text.split('|').collect();
    assert_eq!(parts.len(), 9, "{text}");
    assert_eq!(parts[0], "<@7>");
    assert_eq!(parts[1], "7");
    assert_eq!(parts[2], "tester");
    assert_eq!(parts[3], "<#5>");
    assert_eq!(parts[4], "Templates");
    assert!(parts[5].contains("bad"), "{}", parts[5]);
    assert!(
        parts[6].starts_with(&format!("https://discord.com/channels/{GUILD}/{CHANNEL}/")),
        "{}",
        parts[6]
    );
    assert_eq!(parts[7], GUILD.to_string());
    assert_eq!(parts[8], "1", "one message was matched");
}

#[tokio::test]
async fn the_reason_of_a_punishment_is_the_template_given() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "kick", "reason": "kicked for {rule}" }
    ])));
    say(&harness, MEMBER, "spoiler").await;
    let cases = cases_of(&harness, MEMBER).await;
    assert_eq!(cases[0].reason.as_deref(), Some("kicked for spoilers"));
}

#[tokio::test]
async fn a_punishment_without_a_reason_says_the_rule_and_what_was_found() {
    let harness = with_rules(spoiler_rule(json!([{ "do": "warn" }])));
    say(&harness, MEMBER, "spoiler").await;
    let reason = cases_of(&harness, MEMBER).await[0]
        .reason
        .clone()
        .unwrap_or_default();
    assert!(
        reason.contains("spoilers") && reason.contains("spoiler"),
        "{reason}"
    );
}

// ---- edits ----

#[tokio::test]
async fn an_edited_message_is_checked_and_an_update_without_content_is_ignored() {
    let harness = with_rules(spoiler_rule(delete()));
    let mut edit = custom(MEMBER, CHANNEL, "now a spoiler");
    edit.edited_at = Some(1);
    harness.emit(Event::MessageUpdated(edit)).await.unwrap();
    assert_eq!(deletions(&harness), 1);

    let mut empty = custom(MEMBER, CHANNEL, "");
    empty.edited_at = Some(1);
    harness.emit(Event::MessageUpdated(empty)).await.unwrap();
    assert_eq!(
        deletions(&harness),
        1,
        "an update with neither content nor attachments is skipped"
    );
}

#[tokio::test]
async fn an_action_that_is_not_known_is_refused_by_the_schema_not_run() {
    let harness = with_rules(spoiler_rule(json!([{ "do": "explode" }])));
    say(&harness, MEMBER, "spoiler").await;
    assert!(harness.api().actions().is_empty());
}
