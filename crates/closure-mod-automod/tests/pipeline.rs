//! The pipeline as a whole: the manifest, the schema, the scope of a rule, the order and the
//! cooldown of rules, dry run, invalid rules, and `/automod status`.
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

use std::sync::Arc;
use std::time::Duration;

use closure_config::{ConfigCompiler, parse_overrides};
use closure_core::action::Action;
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use closure_i18n::missing_keys;
use closure_mod_automod::module;
use closure_sdk::testing::{Invocation, Message};
use closure_sdk::{EventKind, Level};
use common::*;
use serde_json::{Value, json};
use twilight_model::gateway::Intents;
use twilight_model::id::Id;

fn words(word: &str) -> Value {
    json!({ "type": "words", "words": [word] })
}

fn spoiler(actions: Value) -> Value {
    rule(json!([words("spoiler")]), actions)
}

async fn status_text(harness: &closure_sdk::testing::Harness) -> String {
    let replies = harness
        .invoke(
            Invocation::new("automod")
                .subcommand("status")
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    description(&replies)
}

// ---- the module as the host sees it ----

#[test]
fn the_manifest_declares_the_intents_capabilities_and_subscriptions() {
    let manifest = module().unwrap().manifest().clone();
    assert_eq!(manifest.id.as_str(), "automod");
    assert!(
        !manifest.default_enabled,
        "automod is off until a server turns it on"
    );
    assert!(
        manifest
            .intents
            .contains(Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT)
    );
    for kind in [EventKind::MessageCreated, EventKind::MessageUpdated] {
        assert!(manifest.subscription(kind).is_some(), "{kind:?}");
    }
    let created = manifest.subscription(EventKind::MessageCreated).unwrap();
    assert!(
        !created.filter.ignore_bots,
        "a rule decides about bots itself"
    );
    for capability in [
        closure_sdk::Capability::Cases,
        closure_sdk::Capability::SendMessages,
        closure_sdk::Capability::ManageMessages,
        closure_sdk::Capability::SendDirectMessages,
        closure_sdk::Capability::KickMembers,
        closure_sdk::Capability::BanMembers,
        closure_sdk::Capability::TimeoutMembers,
        closure_sdk::Capability::ManageRoles,
        closure_sdk::Capability::Storage,
        closure_sdk::Capability::Schedule,
        closure_sdk::Capability::ReadGuild,
        closure_sdk::Capability::ReadMessageHistory,
    ] {
        assert!(manifest.capabilities.contains(capability), "{capability:?}");
    }
    let view = manifest
        .permissions
        .iter()
        .find(|spec| spec.key.as_str() == "automod.view")
        .expect("automod.view is declared");
    assert_eq!(view.default_level, Level::MODERATOR);
}

#[test]
fn the_settings_default_to_no_rules_and_no_dry_run() {
    let manifest = module().unwrap().manifest().clone();
    assert_eq!(
        manifest.default_config,
        json!({
            "dry_run": false,
            "warn_expire_days": 30,
            "dm_on_action": true,
            "timeout_method": "discord",
            "timeout_role": null,
            "rules": {}
        })
    );
}

#[test]
fn the_schema_accepts_a_full_example_and_rejects_unknown_keys_and_bad_shapes() {
    let registry = Arc::new(
        ModuleRegistry::new(vec![Arc::new(module().unwrap()) as Arc<dyn Module>]).unwrap(),
    );
    let compiler = ConfigCompiler::new(registry).unwrap();
    let accepted = |config: Value| {
        let yaml = format!(
            "modules:\n  automod:\n    config: {}\n",
            serde_json::to_string(&config).unwrap()
        );
        compiler.compile(&parse_overrides(&yaml).unwrap()).is_ok()
    };
    let example = json!({
        "dry_run": true,
        "warn_expire_days": 7,
        "dm_on_action": false,
        "timeout_method": "role",
        "timeout_role": "77",
        "rules": {
            "links_1": {
                "name": "Links",
                "enabled": true,
                "triggers": [
                    { "type": "links", "mode": "allowlist", "domains": ["example.com"],
                      "include_subdomains": true, "only_real_links": true,
                      "include_regex": ["/x"], "exclude_regex": ["y"] },
                    { "type": "any_message" }
                ],
                "scope": {
                    "channels": ["5"], "exclude_channels": ["6"], "roles": ["7"],
                    "exclude_roles": ["8"], "include_bots": false, "exempt_moderators": true,
                    "member_age": { "min_days": 1, "max_days": 365 }
                },
                "cooldown": "10m",
                "continue_after_match": false,
                "actions": [{ "do": "delete" }, { "do": "alert", "channel": "60", "text": "{user}" }]
            },
            "words_2": {
                "triggers": [{ "type": "words", "words": ["bad"], "case_sensitive": false,
                               "whole_words": true, "normalize": true, "strip_markdown": true,
                               "targets": ["message", "username"] }],
                "actions": [
                    { "do": "warn", "reason": "{rule}" },
                    { "do": "timeout", "duration": "1h", "reason": "t" },
                    { "do": "ban", "duration": "7d", "delete_days": 1 },
                    { "do": "kick" },
                    { "do": "purge", "within": "10m" },
                    { "do": "reply", "text": "hi {user}", "auto_delete": "30s" }
                ]
            },
            "regex_3": {
                "triggers": [{ "type": "regex", "patterns": ["a+b"], "case_sensitive": true,
                               "normalize": false, "strip_markdown": false, "targets": ["message"] }],
                "actions": [{ "do": "delete" }]
            },
            "invites_4": {
                "triggers": [{ "type": "invites", "allow_codes": ["abc"], "block_codes": ["bad"] }],
                "actions": [{ "do": "delete" }]
            },
            "files_5": {
                "triggers": [{ "type": "attachments", "min_count": 1, "max_count": 3,
                               "blocked_extensions": ["exe"], "allowed_extensions": ["png"],
                               "blocked_mime_types": ["image/*"], "allowed_mime_types": ["image/png"] }],
                "actions": [{ "do": "delete" }]
            },
            "spam_6": {
                "triggers": [
                    { "type": "spam", "measure": "messages", "amount": 5, "within": "1m",
                      "per_channel": false, "distinct_channels": 2,
                      "ignore_identical_messages": true, "group_identical_messages": false },
                    { "type": "spam", "measure": "characters", "amount": 500 }
                ],
                "actions": [{ "do": "timeout", "duration": "10m" }]
            },
            "shape_7": {
                "triggers": [
                    { "type": "caps", "max_ratio": 0.8, "min_letters": 10 },
                    { "type": "repeat", "max_run": 5 },
                    { "type": "stickers", "max": 3 },
                    { "type": "words", "words": ["x"] }
                ],
                "actions": [{ "do": "delete" }]
            }
        }
    });
    assert!(accepted(example.clone()));
    assert!(accepted(json!({})));

    let mut unknown_top = example.clone();
    unknown_top["expire"] = json!(3);
    assert!(!accepted(unknown_top));
    let mut unknown_rule_key = example.clone();
    unknown_rule_key["rules"]["links_1"]["extra"] = json!(true);
    assert!(!accepted(unknown_rule_key));
    let mut unknown_trigger_key = example.clone();
    unknown_trigger_key["rules"]["links_1"]["triggers"][0]["extra"] = json!(true);
    assert!(!accepted(unknown_trigger_key));
    let mut unknown_scope_key = example.clone();
    unknown_scope_key["rules"]["links_1"]["scope"]["extra"] = json!(true);
    assert!(!accepted(unknown_scope_key));
    assert!(!accepted(json!({ "warn_expire_days": 0 })));
    assert!(!accepted(json!({ "dry_run": "yes" })));
    assert!(!accepted(
        json!({ "rules": { "Bad Id": { "triggers": [{ "type": "any_message" }], "actions": [{ "do": "delete" }] } } })
    ));
    assert!(!accepted(
        json!({ "rules": { "x": { "triggers": [], "actions": [{ "do": "delete" }] } } })
    ));
    assert!(!accepted(
        json!({ "rules": { "x": { "triggers": [{ "type": "any_message" }], "actions": [] } } })
    ));
    assert!(!accepted(
        json!({ "rules": { "x": { "triggers": [{ "type": "any_message" }], "actions": [{ "do": "explode" }] } } })
    ));
    assert!(
        !accepted(
            json!({ "rules": [{ "triggers": [{ "type": "any_message" }], "actions": [{ "do": "delete" }] }] })
        ),
        "rules are an object keyed by id"
    );
}

#[test]
fn every_text_is_in_both_languages() {
    let en = include_str!("../locales/en.ftl");
    let ja = include_str!("../locales/ja.ftl");
    assert_eq!(missing_keys(en, ja).unwrap(), Vec::<String>::new());
}

// ---- scope ----

#[tokio::test]
async fn the_channels_of_a_rule_are_included_or_excluded() {
    let included = with_rules(rules(&[(
        "c",
        scoped(
            rule(json!([words("spoiler")]), delete()),
            json!({ "channels": [CHANNEL.to_string()] }),
        ),
    )]));
    say_in(&included, MEMBER, OTHER_CHANNEL, "spoiler").await;
    assert_eq!(deletions(&included), 0);
    say(&included, MEMBER, "spoiler").await;
    assert_eq!(deletions(&included), 1);

    let excluded = with_rules(rules(&[(
        "c",
        scoped(
            rule(json!([words("spoiler")]), delete()),
            json!({ "exclude_channels": [OTHER_CHANNEL.to_string()] }),
        ),
    )]));
    say_in(&excluded, MEMBER, OTHER_CHANNEL, "spoiler").await;
    assert_eq!(deletions(&excluded), 0);
    say(&excluded, MEMBER, "spoiler").await;
    assert_eq!(deletions(&excluded), 1);
}

#[tokio::test]
async fn the_roles_of_a_rule_are_included_or_excluded() {
    let included = with_rules(rules(&[(
        "r",
        scoped(
            rule(json!([words("spoiler")]), delete()),
            json!({ "roles": [ROLE.to_string()] }),
        ),
    )]));
    say(&included, MEMBER, "spoiler").await;
    assert_eq!(deletions(&included), 0);
    included
        .message(
            Message::new(MEMBER, CHANNEL)
                .content("spoiler")
                .roles([ROLE]),
        )
        .await
        .unwrap();
    assert_eq!(deletions(&included), 1);

    let excluded = with_rules(rules(&[(
        "r",
        scoped(
            rule(json!([words("spoiler")]), delete()),
            json!({ "exclude_roles": [ROLE.to_string()] }),
        ),
    )]));
    excluded
        .message(
            Message::new(MEMBER, CHANNEL)
                .content("spoiler")
                .roles([ROLE]),
        )
        .await
        .unwrap();
    assert_eq!(deletions(&excluded), 0);
    say(&excluded, MEMBER, "spoiler").await;
    assert_eq!(deletions(&excluded), 1);
}

#[tokio::test]
async fn bots_are_left_alone_unless_the_rule_includes_them() {
    let plain = with_rules(rules(&[("b", rule(json!([words("spoiler")]), delete()))]));
    plain
        .message(Message::new(MEMBER, CHANNEL).content("spoiler").bot())
        .await
        .unwrap();
    assert_eq!(deletions(&plain), 0);

    let bots = with_rules(rules(&[(
        "b",
        scoped(
            rule(json!([words("spoiler")]), delete()),
            json!({ "include_bots": true }),
        ),
    )]));
    bots.message(Message::new(MEMBER, CHANNEL).content("spoiler").bot())
        .await
        .unwrap();
    assert_eq!(deletions(&bots), 1);
}

#[tokio::test]
async fn moderators_are_exempt_unless_the_rule_says_otherwise() {
    let exempt = with_rules(rules(&[("m", rule(json!([words("spoiler")]), delete()))]));
    exempt
        .message(
            Message::new(MODERATOR, CHANNEL)
                .content("spoiler")
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    assert_eq!(deletions(&exempt), 0);
    say(&exempt, MEMBER, "spoiler").await;
    assert_eq!(deletions(&exempt), 1);

    let strict = with_rules(rules(&[(
        "m",
        scoped(
            rule(json!([words("spoiler")]), delete()),
            json!({ "exempt_moderators": false }),
        ),
    )]));
    strict
        .message(
            Message::new(MODERATOR, CHANNEL)
                .content("spoiler")
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    assert_eq!(deletions(&strict), 1);
}

#[tokio::test]
async fn the_owner_and_the_bot_itself_are_never_checked() {
    let harness = with_rules(rules(&[(
        "o",
        scoped(
            rule(json!([words("spoiler")]), delete()),
            json!({ "include_bots": true, "exempt_moderators": false }),
        ),
    )]));
    harness.api().set_owner(Id::new(GUILD), Id::new(MEMBER));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 0, "the owner is skipped");

    harness
        .api()
        .set_owner(Id::new(GUILD), Id::new(999_999_999));
    harness
        .message(Message::new(BOT, CHANNEL).content("spoiler").bot())
        .await
        .unwrap();
    assert_eq!(deletions(&harness), 0, "the bot's own messages are skipped");
}

#[tokio::test]
async fn member_age_puts_only_members_of_the_given_age_in_scope() {
    let young_only = scoped(
        rule(json!([words("spoiler")]), delete()),
        json!({ "member_age": { "max_days": 7 } }),
    );
    let harness = with_rules(rules(&[("age", young_only)]));
    harness.api().set_member(
        Id::new(GUILD),
        member(MEMBER, &[], false, Some(&days_ago(1))),
    );
    harness.api().set_member(
        Id::new(GUILD),
        member(OTHER_MEMBER, &[], false, Some(&days_ago(60))),
    );
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 1, "joined a day ago");
    say(&harness, OTHER_MEMBER, "spoiler").await;
    assert_eq!(
        deletions(&harness),
        1,
        "joined 60 days ago is too old for this rule"
    );
}

#[tokio::test]
async fn member_age_with_a_minimum_leaves_the_new_members_out() {
    let veterans = scoped(
        rule(json!([words("spoiler")]), delete()),
        json!({ "member_age": { "min_days": 30 } }),
    );
    let harness = with_rules(rules(&[("age", veterans)]));
    harness.api().set_member(
        Id::new(GUILD),
        member(MEMBER, &[], false, Some(&days_ago(1))),
    );
    harness.api().set_member(
        Id::new(GUILD),
        member(OTHER_MEMBER, &[], false, Some(&days_ago(60))),
    );
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 0);
    say(&harness, OTHER_MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 1);
}

#[tokio::test]
async fn a_member_whose_join_date_is_unknown_is_out_of_scope_of_an_age_rule() {
    let harness = with_rules(rules(&[(
        "age",
        scoped(
            rule(json!([words("spoiler")]), delete()),
            json!({ "member_age": { "min_days": 1 } }),
        ),
    )]));
    harness
        .api()
        .set_not_a_member(Id::new(GUILD), Id::new(MEMBER));
    harness
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await
        .unwrap();
    assert_eq!(
        deletions(&harness),
        0,
        "not known, so not in scope; nothing fails"
    );
}

// ---- order, continuation, cooldown ----

#[tokio::test]
async fn the_first_rule_that_hits_is_the_only_one_that_fires() {
    let harness = with_rules(rules(&[
        ("first", rule(json!([words("spoiler")]), delete())),
        (
            "second",
            rule(json!([words("spoiler")]), json!([{ "do": "kick" }])),
        ),
    ]));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 1);
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::Kick { .. })),
        0
    );
}

#[tokio::test]
async fn rules_are_checked_in_the_order_of_their_ids() {
    let harness = with_rules(rules(&[
        ("b_rule", rule(json!([words("spoiler")]), delete())),
        (
            "a_rule",
            rule(json!([words("spoiler")]), json!([{ "do": "kick" }])),
        ),
    ]));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(
        count_actions(&harness, |a| matches!(a, Action::Kick { .. })),
        1,
        "a_rule comes first"
    );
    assert_eq!(deletions(&harness), 0);
}

#[tokio::test]
async fn continue_after_match_lets_the_later_rules_run_too() {
    let harness = with_rules(rules(&[
        (
            "a",
            json!({ "triggers": [words("spoiler")], "actions": [{ "do": "warn" }], "continue_after_match": true }),
        ),
        ("b", rule(json!([words("spoiler")]), delete())),
    ]));
    say(&harness, MEMBER, "spoiler").await;
    let warnings = cases_of(&harness, MEMBER)
        .await
        .into_iter()
        .filter(|case| case.kind.as_str() == "warn")
        .count();
    assert_eq!(warnings, 1, "a's warning");
    assert_eq!(deletions(&harness), 1);
}

#[tokio::test]
async fn without_continue_after_match_a_hit_ends_the_evaluation() {
    let harness = with_rules(rules(&[
        (
            "a",
            rule(json!([words("spoiler")]), json!([{ "do": "warn" }])),
        ),
        ("b", rule(json!([words("spoiler")]), delete())),
    ]));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
    assert_eq!(deletions(&harness), 0);
}

#[tokio::test]
async fn a_cooldown_lets_a_rule_fire_once_per_member_for_its_length() {
    let harness = with_rules(rules(&[(
        "cool",
        json!({ "triggers": [words("spoiler")], "actions": [{ "do": "warn" }], "cooldown": "1h" }),
    )]));
    say(&harness, MEMBER, "spoiler").await;
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(
        cases_of(&harness, MEMBER).await.len(),
        1,
        "the second one is on cooldown"
    );

    say(&harness, OTHER_MEMBER, "spoiler").await;
    assert_eq!(
        cases_of(&harness, OTHER_MEMBER).await.len(),
        1,
        "the cooldown is per member"
    );

    harness.advance(Duration::from_secs(2 * 3600));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(
        cases_of(&harness, MEMBER).await.len(),
        2,
        "after the cooldown it fires again"
    );
}

#[tokio::test]
async fn a_rule_on_cooldown_is_skipped_and_the_next_rule_gets_its_turn() {
    let harness = with_rules(rules(&[
        (
            "a",
            json!({ "triggers": [words("spoiler")], "actions": [{ "do": "warn" }], "cooldown": "1h" }),
        ),
        ("b", rule(json!([words("spoiler")]), delete())),
    ]));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 0, "a fired and ended the evaluation");
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 1, "a is on cooldown, so b deletes");
}

// ---- dry run ----

#[tokio::test]
async fn a_dry_run_acts_on_nothing_and_posts_what_would_have_run() {
    let harness = configured(json!({
        "dry_run": true,
        "rules": rules(&[("dry", spoiler(json!([{ "do": "delete" }, { "do": "warn" }])))])
    }));
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 0);
    assert_eq!(
        count_actions(&harness, |a| matches!(
            a,
            Action::Ban { .. } | Action::Timeout { .. } | Action::Kick { .. }
        )),
        0
    );
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), 1, "one embed, in the log");
    let text = embed_text(&embeds[0]);
    assert!(text.contains("[dry run] automod"), "{text}");
    assert!(text.contains("dry"), "{text}");
    assert!(text.contains("delete") && text.contains("warn"), "{text}");
}

#[tokio::test]
async fn a_dry_run_without_a_log_channel_does_nothing_at_all() {
    let harness = harness();
    harness.set_config(json!({ "dry_run": true, "rules": rules(&[("dry", spoiler(delete()))]) }));
    say(&harness, MEMBER, "spoiler").await;
    assert!(harness.api().actions().is_empty());
}

#[tokio::test]
async fn a_dry_run_does_not_take_the_burst_so_every_message_of_it_is_reported() {
    let harness = configured(json!({
        "dry_run": true,
        "rules": rules(&[("flood", rule(json!([{ "type": "spam", "measure": "messages", "amount": 2, "within": "10s" }]), delete()))])
    }));
    for content in ["a", "b", "c"] {
        say(&harness, MEMBER, content).await;
    }
    assert_eq!(
        harness.sent_embeds().len(),
        2,
        "the second and the third message would both hit"
    );
    assert_eq!(deletions(&harness), 0);
}

// ---- invalid rules ----

#[tokio::test]
async fn invalid_rules_never_run_and_the_status_lists_them() {
    let harness = configured(json!({
        "rules": {
            "ok": spoiler(delete()),
            "bad_type": { "triggers": [{ "type": "regex" }], "actions": [{ "do": "delete" }] },
            "Bad Id": { "triggers": [words("other")], "actions": [{ "do": "delete" }] },
            "no_words": { "triggers": [{ "type": "words", "words": [] }], "actions": [{ "do": "delete" }] },
            "no_triggers": { "triggers": [], "actions": [{ "do": "delete" }] },
            "no_actions": { "triggers": [words("x")], "actions": [] },
            "extra": { "triggers": [{ "type": "any_message", "extra": 1 }], "actions": [{ "do": "delete" }] },
            "bad_regex": { "triggers": [{ "type": "regex", "patterns": ["("] }], "actions": [{ "do": "delete" }] },
            "big_regex": { "triggers": [{ "type": "regex", "patterns": ["(a{1000}){1000}"] }], "actions": [{ "do": "delete" }] },
            "bad_template": { "triggers": [words("x")], "actions": [{ "do": "reply", "text": "{user" }] },
            "long_window": { "triggers": [{ "type": "spam", "measure": "messages", "amount": 2, "within": "2h" }], "actions": [{ "do": "delete" }] },
            "no_time": { "triggers": [words("x")], "actions": [{ "do": "timeout" }] }
        }
    }));
    say(&harness, MEMBER, "xxxx").await;
    assert_eq!(
        deletions(&harness),
        0,
        "only `ok` is valid, and it looks for spoiler"
    );
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 1);

    let text = status_text(&harness).await;
    for label in [
        "bad_type",
        "Bad Id",
        "no_words",
        "no_triggers",
        "no_actions",
        "extra",
        "bad_regex",
        "big_regex",
        "bad_template",
        "long_window",
        "no_time",
    ] {
        assert!(text.contains(label), "{label} is listed: {text}");
    }
    assert!(text.contains("11 invalid"), "{text}");
}

// ---- /automod status ----

#[tokio::test]
async fn status_shows_the_dry_run_state_the_counts_and_each_rule_in_one_line() {
    let harness = configured(json!({
        "rules": {
            "spoilers": {
                "name": "Spoilers",
                "triggers": [words("spoiler")],
                "actions": [{ "do": "delete" }, { "do": "warn" }],
                "cooldown": "1h"
            },
            "links": {
                "enabled": false,
                "triggers": [{ "type": "links", "mode": "any" }],
                "actions": [{ "do": "alert", "channel": NOTIFY.to_string() }]
            },
            "broken": { "triggers": [{ "type": "regex" }], "actions": [{ "do": "delete" }] }
        }
    }));
    let replies = harness
        .invoke(
            Invocation::new("automod")
                .subcommand("status")
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    let text = description(&replies);
    assert!(text.contains("Dry run: off"), "{text}");
    assert!(
        text.contains("Rules: 1 enabled, 1 disabled, 1 invalid."),
        "{text}"
    );
    assert!(
        text.contains("spoilers") && text.contains("Spoilers"),
        "{text}"
    );
    assert!(
        text.contains("words") && text.contains("delete, warn"),
        "{text}"
    );
    assert!(text.contains("links") && text.contains("alert"), "{text}");
    assert!(text.contains("broken"), "{text}");
    assert!(
        replies.iter().all(|reply| reply.ephemeral),
        "the status is private"
    );
}

#[tokio::test]
async fn status_needs_the_view_permission() {
    let harness = configured(json!({}));
    let replies = harness
        .invoke(Invocation::new("automod").subcommand("status"))
        .await
        .unwrap();
    assert!(
        replies
            .iter()
            .flat_map(|reply| reply.embeds.iter())
            .all(|embed| embed.title.as_deref() != Some("Automod status")),
        "nothing is shown to a member"
    );
    assert!(!replies.is_empty(), "the refusal is answered");
}

#[tokio::test]
async fn the_shipped_example_configuration_has_no_invalid_rule() {
    let harness = configured(json!({
        "rules": {
            "links_1": {
                "triggers": [{ "type": "links", "mode": "allowlist", "domains": ["example.com"] }],
                "scope": { "channels": ["5"] },
                "actions": [{ "do": "delete" }, { "do": "alert", "channel": "60" }]
            },
            "words_2": {
                "triggers": [{ "type": "words", "words": ["bad"] }],
                "actions": [{ "do": "warn" }, { "do": "timeout", "duration": "1h" }, { "do": "ban", "duration": "7d", "delete_days": 1 }, { "do": "kick" }]
            },
            "spam_9": {
                "triggers": [{ "type": "spam", "measure": "messages", "amount": 5, "within": "1m" }],
                "cooldown": "10m",
                "actions": [{ "do": "purge", "within": "1m" }]
            }
        }
    }));
    assert!(status_text(&harness).await.contains("0 invalid"));
}

#[tokio::test]
async fn a_status_line_of_a_rule_names_its_triggers_and_actions() {
    let harness = configured(json!({
        "rules": { "x": { "triggers": [{ "type": "repeat", "max_run": 3 }], "actions": [{ "do": "purge", "within": "1m" }] } }
    }));
    let text = status_text(&harness).await;
    assert!(text.contains("repeat"), "{text}");
    assert!(text.contains("purge"), "{text}");
}
