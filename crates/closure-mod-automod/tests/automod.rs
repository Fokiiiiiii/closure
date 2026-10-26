//! The automod module, run against the SDK's pretend Discord.
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

use std::sync::Arc;
use std::time::Duration;

use closure_config::{ConfigCompiler, parse_overrides};
use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use closure_i18n::missing_keys;
use closure_mod_automod::{builder, module};
use closure_sdk::testing::{Harness, Invocation, Message, Reply};
use closure_sdk::{AttachmentInfo, Case, Event, EventKind, Level, MessageInfo, UserInfo};
use serde_json::{Value, json};
use twilight_model::channel::message::Embed;
use twilight_model::gateway::Intents;
use twilight_model::id::Id;

const GUILD: u64 = 1;
const CHANNEL: u64 = 5;
const OTHER_CHANNEL: u64 = 6;
const LOG: u64 = 50;
const NOTIFY: u64 = 60;
const MODERATOR_ROLE: u64 = 10;
const ROLE: u64 = 77;
const MODERATOR: u64 = 100;
const MEMBER: u64 = 7;
/// The bot's own user id, as the harness gives it.
const BOT: u64 = 9_999;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR_ROLE), Level::MODERATOR);
    });
    for channel in [CHANNEL, OTHER_CHANNEL, NOTIFY] {
        harness
            .api()
            .add_channel(Id::new(channel), Some(Id::new(GUILD)));
    }
    harness
}

/// A harness with the log channel set and `config` as the module's configuration.
fn configured(config: Value) -> Harness {
    let harness = harness();
    harness.set_log_channel(LOG);
    harness.set_config(config);
    harness
}

/// A harness with one rule, or several, in its configuration.
fn with_rules(rules: Value) -> Harness {
    configured(json!({ "rules": rules }))
}

fn rule(id: &str, check: Value, actions: Value) -> Value {
    json!({ "id": id, "check": check, "actions": actions })
}

fn scoped(mut rule: Value, scope: Value) -> Value {
    rule["scope"] = scope;
    rule
}

fn delete() -> Value {
    json!([{ "do": "delete" }])
}

/// Posts `content` from `author` in the channel `CHANNEL`.
async fn say(harness: &Harness, author: u64, content: &str) {
    harness
        .message(Message::new(author, CHANNEL).content(content))
        .await
        .unwrap();
}

fn count_actions(harness: &Harness, wanted: impl Fn(&Action) -> bool) -> usize {
    harness.api().actions().iter().filter(|a| wanted(a)).count()
}

fn deletions(harness: &Harness) -> usize {
    count_actions(harness, |a| matches!(a, Action::DeleteMessage { .. }))
}

async fn cases_of(harness: &Harness, target: u64) -> Vec<Case> {
    harness
        .cases()
        .for_target(Id::new(target), None, 25)
        .await
        .unwrap()
}

/// Everything a embed says, as one text.
fn embed_text(embed: &Embed) -> String {
    let mut text = String::new();
    if let Some(title) = &embed.title {
        text.push_str(title);
    }
    if let Some(description) = &embed.description {
        text.push('\n');
        text.push_str(description);
    }
    for field in &embed.fields {
        text.push('\n');
        text.push_str(&field.name);
        text.push(' ');
        text.push_str(&field.value);
    }
    text
}

fn description(replies: &[Reply]) -> String {
    replies
        .iter()
        .flat_map(|reply| reply.embeds.iter())
        .filter_map(|embed| embed.description.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// An edit of a message, with its content and no attachments.
fn update(author: u64, content: &str) -> MessageInfo {
    MessageInfo {
        id: Id::new(900),
        channel_id: Id::new(CHANNEL),
        guild_id: Id::new(GUILD),
        author: UserInfo {
            id: Id::new(author),
            name: "tester".into(),
            bot: false,
        },
        author_roles: Vec::new(),
        content: content.to_owned(),
        mentioned_users: Vec::new(),
        mentioned_roles: Vec::new(),
        mentions_everyone: false,
        attachments: Vec::<AttachmentInfo>::new(),
        embed_count: 0,
        sticker_count: 0,
        reply_to: None,
        from_webhook: false,
        created_at: 0,
        edited_at: Some(1),
    }
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
            "rules": []
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
        "rules": [
            {
                "id": "links_1",
                "name": "Links",
                "enabled": true,
                "check": { "type": "links", "mode": "allowlist", "domains": ["example.com"] },
                "scope": {
                    "channels": ["5"], "exclude_channels": ["6"], "roles": ["7"],
                    "exclude_roles": ["8"], "include_bots": false, "exempt_moderators": true
                },
                "actions": [{ "do": "delete" }, { "do": "notify", "channel": "60" }]
            },
            { "id": "words_2", "check": { "type": "words", "words": ["bad"], "match": "whole" },
              "actions": [{ "do": "warn" }, { "do": "timeout", "duration": "1h" },
                          { "do": "ban", "duration": "7d", "delete_days": 1 }, { "do": "kick" }] },
            { "id": "invites_3", "check": { "type": "invites", "allow": ["abc"] },
              "actions": [{ "do": "delete" }] },
            { "id": "mentions_4", "check": { "type": "mentions", "max": 5 }, "actions": [{ "do": "delete" }] },
            { "id": "caps_5", "check": { "type": "caps", "max_ratio": 0.8, "min_letters": 10 },
              "actions": [{ "do": "delete" }] },
            { "id": "repeat_6", "check": { "type": "repeat", "max_run": 5 }, "actions": [{ "do": "delete" }] },
            { "id": "emoji_7", "check": { "type": "emoji", "max": 10 }, "actions": [{ "do": "delete" }] },
            { "id": "files_8", "check": { "type": "attachments", "max": 3, "blocked_extensions": ["exe"] },
              "actions": [{ "do": "delete" }] },
            { "id": "flood_9", "check": { "type": "flood", "messages": 5, "per_minutes": 1 },
              "actions": [{ "do": "timeout", "duration": "10m" }] }
        ]
    });
    assert!(accepted(example.clone()));
    assert!(accepted(json!({})));

    let mut unknown_top = example.clone();
    unknown_top["expire"] = json!(3);
    assert!(!accepted(unknown_top));
    let mut unknown_rule_key = example.clone();
    unknown_rule_key["rules"][0]["extra"] = json!(true);
    assert!(!accepted(unknown_rule_key));
    let mut unknown_check_key = example.clone();
    unknown_check_key["rules"][0]["check"]["extra"] = json!(true);
    assert!(!accepted(unknown_check_key));
    assert!(!accepted(json!({ "warn_expire_days": 0 })));
    assert!(!accepted(json!({ "dry_run": "yes" })));
    assert!(!accepted(
        json!({ "rules": [{ "id": "Bad Id", "check": { "type": "repeat", "max_run": 3 }, "actions": [{ "do": "delete" }] }] })
    ));
    assert!(!accepted(
        json!({ "rules": [{ "id": "x", "check": { "type": "repeat", "max_run": 3 }, "actions": [] }] })
    ));
    assert!(!accepted(
        json!({ "rules": [{ "id": "x", "check": { "type": "repeat", "max_run": 3 }, "actions": [{ "do": "explode" }] }] })
    ));
}

#[test]
fn every_text_is_in_both_languages() {
    let en = include_str!("../locales/en.ftl");
    let ja = include_str!("../locales/ja.ftl");
    assert_eq!(missing_keys(en, ja).unwrap(), Vec::<String>::new());
}

// ---- the checks ----

#[tokio::test]
async fn a_contains_word_rule_matches_japanese_inside_a_sentence() {
    let h = with_rules(json!([rule(
        "ja",
        json!({ "type": "words", "words": ["バカ"], "match": "contains" }),
        delete()
    )]));
    say(&h, MEMBER, "ほんとバカだなあ").await;
    assert_eq!(deletions(&h), 1);
}

#[tokio::test]
async fn a_whole_word_rule_does_not_match_a_japanese_word_inside_a_run() {
    let h = with_rules(json!([rule(
        "ja",
        json!({ "type": "words", "words": ["バカ"], "match": "whole" }),
        delete()
    )]));
    say(&h, MEMBER, "ほんとバカだなあ").await;
    assert_eq!(deletions(&h), 0);
}

#[tokio::test]
async fn a_whole_word_rule_ignores_case_and_width_but_not_a_longer_word() {
    let h = with_rules(json!([rule(
        "ass",
        json!({ "type": "words", "words": ["ass"] }),
        delete()
    )]));
    say(&h, MEMBER, "what an ASS!").await;
    say(&h, MEMBER, "class").await;
    say(&h, MEMBER, "ＡＳＳ").await;
    assert_eq!(deletions(&h), 2);
}

#[tokio::test]
async fn an_any_link_rule_hits_a_link_and_misses_plain_text() {
    let h = with_rules(json!([rule(
        "any",
        json!({ "type": "links", "mode": "any", "domains": [] }),
        delete()
    )]));
    say(&h, MEMBER, "see https://example.com/x").await;
    say(
        &h,
        MEMBER,
        "no link here, example.com without scheme is fine",
    )
    .await;
    assert_eq!(deletions(&h), 1);
}

#[tokio::test]
async fn an_allowlist_hits_a_link_outside_the_list_but_not_a_subdomain_and_not_a_lookalike() {
    let h = with_rules(json!([rule(
        "allow",
        json!({ "type": "links", "mode": "allowlist", "domains": ["example.com"] }),
        delete()
    )]));
    say(&h, MEMBER, "https://shop.example.com/x").await;
    say(&h, MEMBER, "https://example.com").await;
    assert_eq!(deletions(&h), 0);
    say(&h, MEMBER, "https://evil-example.com").await;
    assert_eq!(deletions(&h), 1);
}

#[tokio::test]
async fn a_blocklist_hits_only_listed_domains_and_their_subdomains() {
    let h = with_rules(json!([rule(
        "block",
        json!({ "type": "links", "mode": "blocklist", "domains": ["bad.example"] }),
        delete()
    )]));
    say(&h, MEMBER, "https://bad.example/x").await;
    say(&h, MEMBER, "https://shop.bad.example").await;
    say(&h, MEMBER, "https://example.com").await;
    say(&h, MEMBER, "https://notbad.example").await;
    assert_eq!(deletions(&h), 2);
}

#[tokio::test]
async fn an_invite_rule_hits_only_the_invites_that_are_not_allowed() {
    let h = with_rules(json!([rule(
        "invites",
        json!({ "type": "invites", "allow": ["abcDEF"] }),
        delete()
    )]));
    say(&h, MEMBER, "join discord.gg/abcDEF").await;
    assert_eq!(deletions(&h), 0);
    say(&h, MEMBER, "join discord.gg/Zzz").await;
    assert_eq!(deletions(&h), 1);
}

#[tokio::test]
async fn mentions_count_users_roles_and_everyone_together() {
    let h = with_rules(json!([rule(
        "mentions",
        json!({ "type": "mentions", "max": 2 }),
        delete()
    )]));
    h.message(Message::new(MEMBER, CHANNEL).mention(1).mention(2))
        .await
        .unwrap();
    assert_eq!(deletions(&h), 0, "two mentions are allowed");
    h.message(
        Message::new(MEMBER, CHANNEL)
            .mention(1)
            .mention(2)
            .mention(3),
    )
    .await
    .unwrap();
    assert_eq!(deletions(&h), 1);
    h.message(
        Message::new(MEMBER, CHANNEL)
            .mention(1)
            .mention_role(5)
            .mention_everyone(),
    )
    .await
    .unwrap();
    assert_eq!(deletions(&h), 2, "a user, a role and everyone are three");
}

#[tokio::test]
async fn caps_need_enough_letters_before_they_count() {
    let h = with_rules(json!([rule(
        "caps",
        json!({ "type": "caps", "max_ratio": 0.7, "min_letters": 10 }),
        delete()
    )]));
    say(&h, MEMBER, "HELLO").await;
    assert_eq!(deletions(&h), 0, "five letters are too few to judge");
    say(&h, MEMBER, "HELLO THERE FRIEND").await;
    assert_eq!(deletions(&h), 1);
    say(&h, MEMBER, "Hello there friend").await;
    assert_eq!(deletions(&h), 1);
}

#[tokio::test]
async fn a_repeat_rule_hits_a_run_longer_than_allowed() {
    let h = with_rules(json!([rule(
        "repeat",
        json!({ "type": "repeat", "max_run": 4 }),
        delete()
    )]));
    say(&h, MEMBER, "aaaa").await;
    assert_eq!(deletions(&h), 0);
    say(&h, MEMBER, "aaaaa").await;
    assert_eq!(deletions(&h), 1);
}

#[tokio::test]
async fn an_emoji_rule_counts_unicode_and_custom_emoji() {
    let h = with_rules(json!([rule(
        "emoji",
        json!({ "type": "emoji", "max": 2 }),
        delete()
    )]));
    say(&h, MEMBER, "👍 👍").await;
    assert_eq!(deletions(&h), 0);
    say(&h, MEMBER, "👍👍👍").await;
    assert_eq!(deletions(&h), 1);
    say(&h, MEMBER, "<:wave:42> <:wave:42> <:wave:42>").await;
    assert_eq!(deletions(&h), 2);
}

#[tokio::test]
async fn an_attachment_rule_hits_by_count_and_by_extension() {
    let h = with_rules(json!([rule(
        "files",
        json!({ "type": "attachments", "max": 1, "blocked_extensions": ["exe"] }),
        delete()
    )]));
    h.message(Message::new(MEMBER, CHANNEL).attachment("a.png", 10))
        .await
        .unwrap();
    assert_eq!(deletions(&h), 0);
    h.message(
        Message::new(MEMBER, CHANNEL)
            .attachment("a.png", 10)
            .attachment("b.png", 10),
    )
    .await
    .unwrap();
    assert_eq!(deletions(&h), 1, "two files are more than one");
    h.message(Message::new(MEMBER, CHANNEL).attachment("setup.EXE", 10))
        .await
        .unwrap();
    assert_eq!(
        deletions(&h),
        2,
        "a blocked extension counts whatever its case"
    );
}

#[tokio::test]
async fn a_flood_rule_fires_once_per_burst_and_counts_again_after_the_window() {
    let h = with_rules(json!([rule(
        "flood",
        json!({ "type": "flood", "messages": 3, "per_minutes": 1 }),
        delete()
    )]));
    say(&h, MEMBER, "a").await;
    say(&h, MEMBER, "b").await;
    assert_eq!(deletions(&h), 0);
    say(&h, MEMBER, "c").await;
    assert_eq!(
        deletions(&h),
        1,
        "the third message in the window is a burst"
    );
    say(&h, MEMBER, "d").await;
    assert_eq!(deletions(&h), 1, "the counter starts again after a hit");
    h.advance(Duration::from_secs(120));
    say(&h, MEMBER, "e").await;
    say(&h, MEMBER, "f").await;
    assert_eq!(deletions(&h), 1);
    say(&h, MEMBER, "g").await;
    assert_eq!(deletions(&h), 2);
}

#[tokio::test]
async fn a_flood_is_counted_on_new_messages_only() {
    let h = with_rules(json!([rule(
        "flood",
        json!({ "type": "flood", "messages": 2, "per_minutes": 1 }),
        delete()
    )]));
    say(&h, MEMBER, "a").await;
    for _ in 0..3 {
        h.emit(Event::MessageUpdated(update(MEMBER, "edited")))
            .await
            .unwrap();
    }
    assert_eq!(deletions(&h), 0, "edits do not count towards a burst");
    say(&h, MEMBER, "b").await;
    assert_eq!(deletions(&h), 1);
}

// ---- scope ----

#[tokio::test]
async fn the_channels_of_a_rule_are_included_or_excluded() {
    let included = with_rules(json!([scoped(
        rule(
            "c",
            json!({ "type": "words", "words": ["spoiler"] }),
            delete()
        ),
        json!({ "channels": [CHANNEL.to_string()] })
    )]));
    included
        .message(Message::new(MEMBER, OTHER_CHANNEL).content("spoiler"))
        .await
        .unwrap();
    assert_eq!(deletions(&included), 0);
    included
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await
        .unwrap();
    assert_eq!(deletions(&included), 1);

    let excluded = with_rules(json!([scoped(
        rule(
            "c",
            json!({ "type": "words", "words": ["spoiler"] }),
            delete()
        ),
        json!({ "exclude_channels": [OTHER_CHANNEL.to_string()] })
    )]));
    excluded
        .message(Message::new(MEMBER, OTHER_CHANNEL).content("spoiler"))
        .await
        .unwrap();
    assert_eq!(deletions(&excluded), 0);
    excluded
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await
        .unwrap();
    assert_eq!(deletions(&excluded), 1);
}

#[tokio::test]
async fn the_roles_of_a_rule_are_included_or_excluded() {
    let included = with_rules(json!([scoped(
        rule(
            "r",
            json!({ "type": "words", "words": ["spoiler"] }),
            delete()
        ),
        json!({ "roles": [ROLE.to_string()] })
    )]));
    included
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await
        .unwrap();
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

    let excluded = with_rules(json!([scoped(
        rule(
            "r",
            json!({ "type": "words", "words": ["spoiler"] }),
            delete()
        ),
        json!({ "exclude_roles": [ROLE.to_string()] })
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
    excluded
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await
        .unwrap();
    assert_eq!(deletions(&excluded), 1);
}

#[tokio::test]
async fn bots_are_left_alone_unless_the_rule_includes_them() {
    let plain = with_rules(json!([rule(
        "b",
        json!({ "type": "words", "words": ["spoiler"] }),
        delete()
    )]));
    plain
        .message(Message::new(MEMBER, CHANNEL).content("spoiler").bot())
        .await
        .unwrap();
    assert_eq!(deletions(&plain), 0);

    let bots = with_rules(json!([scoped(
        rule(
            "b",
            json!({ "type": "words", "words": ["spoiler"] }),
            delete()
        ),
        json!({ "include_bots": true })
    )]));
    bots.message(Message::new(MEMBER, CHANNEL).content("spoiler").bot())
        .await
        .unwrap();
    assert_eq!(deletions(&bots), 1);
}

#[tokio::test]
async fn moderators_are_exempt_unless_the_rule_says_otherwise() {
    let exempt = with_rules(json!([rule(
        "m",
        json!({ "type": "words", "words": ["spoiler"] }),
        delete()
    )]));
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

    let strict = with_rules(json!([scoped(
        rule(
            "m",
            json!({ "type": "words", "words": ["spoiler"] }),
            delete()
        ),
        json!({ "exempt_moderators": false })
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
    let h = with_rules(json!([scoped(
        rule(
            "o",
            json!({ "type": "words", "words": ["spoiler"] }),
            delete()
        ),
        json!({ "include_bots": true, "exempt_moderators": false })
    )]));
    h.api().set_owner(Id::new(GUILD), Id::new(MEMBER));
    say(&h, MEMBER, "spoiler").await;
    assert_eq!(deletions(&h), 0, "the owner is skipped");

    h.api().set_owner(Id::new(GUILD), Id::new(999_999_999));
    h.message(Message::new(BOT, CHANNEL).content("spoiler").bot())
        .await
        .unwrap();
    assert_eq!(deletions(&h), 0, "the bot's own messages are skipped");
}

// ---- the pipeline ----

#[tokio::test]
async fn the_first_rule_that_hits_is_the_only_one_that_fires() {
    let h = with_rules(json!([
        rule(
            "first",
            json!({ "type": "words", "words": ["spoiler"] }),
            delete()
        ),
        rule(
            "second",
            json!({ "type": "words", "words": ["spoiler"] }),
            json!([{ "do": "kick" }])
        )
    ]));
    say(&h, MEMBER, "spoiler").await;
    assert_eq!(deletions(&h), 1);
    assert_eq!(count_actions(&h, |a| matches!(a, Action::Kick { .. })), 0);
}

#[tokio::test]
async fn a_failing_action_does_not_stop_the_next_one_and_the_error_is_reported() {
    let h = with_rules(json!([rule(
        "two",
        json!({ "type": "words", "words": ["spoiler"] }),
        json!([{ "do": "delete" }, { "do": "notify", "channel": NOTIFY.to_string() }])
    )]));
    h.api()
        .fail_nth(1, DiscordError::Forbidden("no permission".into()));
    let result = h
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await;
    assert!(result.is_err(), "the first error reaches the dispatcher");
    assert_eq!(
        h.sent_embeds().len(),
        2,
        "the notification ran (one for the rule, one for the log)"
    );
}

#[tokio::test]
async fn a_message_that_is_already_deleted_is_not_an_error() {
    let h = with_rules(json!([rule(
        "gone",
        json!({ "type": "words", "words": ["spoiler"] }),
        delete()
    )]));
    h.api().fail_nth(1, DiscordError::NotFound);
    h.message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await
        .unwrap();
    assert_eq!(
        cases_of(&h, MEMBER).await.len(),
        1,
        "the rule still left its case"
    );
}

// ---- actions ----

#[tokio::test]
async fn delete_removes_the_message_and_records_an_automod_case_that_the_log_shows() {
    let h = configured(
        json!({ "rules": [rule("spoilers", json!({ "type": "words", "words": ["spoiler"] }), delete())] }),
    );
    say(&h, MEMBER, "spoiler").await;
    let reason = h
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

    let cases = cases_of(&h, MEMBER).await;
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].kind.as_str(), "automod");
    assert_eq!(cases[0].moderator, Id::new(BOT));
    assert_eq!(cases[0].expires_at, None);
    let logged = h.api().actions().iter().any(
        |a| matches!(a, Action::SendMessage { channel_id, .. } if *channel_id == Id::new(LOG)),
    );
    assert!(logged, "the case is posted to the log");
}

#[tokio::test]
async fn notify_posts_one_embed_with_the_rule_member_detail_and_a_link() {
    let h = configured(json!({ "rules": [rule(
        "spoilers",
        json!({ "type": "words", "words": ["spoiler"] }),
        json!([{ "do": "notify", "channel": NOTIFY.to_string() }])
    )] }));
    say(&h, MEMBER, "a spoiler here").await;
    let embeds: Vec<String> = h
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                channel_id, embeds, ..
            } if channel_id == Id::new(NOTIFY) => {
                Some(embeds.iter().map(embed_text).collect::<Vec<_>>().join("\n"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(embeds.len(), 1, "one embed in the notify channel");
    let text = &embeds[0];
    assert!(text.contains("spoilers"), "{text}");
    assert!(text.contains("<@7>"), "{text}");
    assert!(text.contains("spoiler"), "{text}");
    assert!(
        text.contains(&format!("https://discord.com/channels/{GUILD}/{CHANNEL}/")),
        "{text}"
    );
}

#[tokio::test]
async fn warn_records_a_warning_that_expires_in_the_configured_days() {
    let h = configured(json!({
        "warn_expire_days": 7,
        "rules": [rule("w", json!({ "type": "words", "words": ["spoiler"] }), json!([{ "do": "warn" }]))]
    }));
    let before = h.cases().recent(None, 1).await.unwrap().len();
    assert_eq!(before, 0);
    say(&h, MEMBER, "spoiler").await;
    let cases = cases_of(&h, MEMBER).await;
    assert_eq!(cases.len(), 1, "a warning and no separate automod case");
    assert_eq!(cases[0].kind.as_str(), "warn");
    assert_eq!(cases[0].moderator, Id::new(BOT));
    let created = cases[0].created_at;
    assert_eq!(cases[0].expires_at, Some(created + 7 * 86_400));
}

#[tokio::test]
async fn a_timeout_by_discord_sets_the_timeout_and_records_a_timeout_case() {
    let h = with_rules(json!([rule(
        "t",
        json!({ "type": "words", "words": ["spoiler"] }),
        json!([{ "do": "timeout", "duration": "1h" }])
    )]));
    say(&h, MEMBER, "spoiler").await;
    assert!(h.api().actions().iter().any(|a| matches!(
        a,
        Action::Timeout { user_id, duration_seconds: Some(3600), .. } if *user_id == Id::new(MEMBER)
    )));
    let cases = cases_of(&h, MEMBER).await;
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].kind.as_str(), "timeout");
}

#[tokio::test]
async fn a_timeout_by_role_adds_the_role_instead() {
    let h = configured(json!({
        "timeout_method": "role",
        "timeout_role": "77",
        "rules": [rule(
            "t",
            json!({ "type": "words", "words": ["spoiler"] }),
            json!([{ "do": "timeout", "duration": "1h" }])
        )]
    }));
    say(&h, MEMBER, "spoiler").await;
    assert!(h.api().actions().iter().any(|a| matches!(
        a,
        Action::AddRole { role_id, .. } if *role_id == Id::new(ROLE)
    )));
    assert_eq!(
        count_actions(&h, |a| matches!(a, Action::Timeout { .. })),
        0
    );
}

#[tokio::test]
async fn kick_kicks_the_member_and_records_a_kick_case() {
    let h = with_rules(json!([rule(
        "k",
        json!({ "type": "words", "words": ["spoiler"] }),
        json!([{ "do": "kick" }])
    )]));
    say(&h, MEMBER, "spoiler").await;
    assert_eq!(count_actions(&h, |a| matches!(a, Action::Kick { .. })), 1);
    assert_eq!(cases_of(&h, MEMBER).await[0].kind.as_str(), "kick");
}

#[tokio::test]
async fn a_timed_ban_deletes_the_days_given_sets_a_timer_and_lifts_the_ban_when_due() {
    let h = with_rules(json!([rule(
        "b",
        json!({ "type": "words", "words": ["spoiler"] }),
        json!([{ "do": "ban", "duration": "1d", "delete_days": 2 }])
    )]));
    say(&h, MEMBER, "spoiler").await;
    assert!(h.api().actions().iter().any(|a| matches!(
        a,
        Action::Ban {
            delete_message_seconds: 172_800,
            ..
        }
    )));
    assert_eq!(cases_of(&h, MEMBER).await[0].kind.as_str(), "ban");
    assert_eq!(h.jobs(), 1, "the timer of the ban");

    h.advance(Duration::from_secs(86_401));
    let ended = h.run_due_jobs().await;
    assert!(ended.iter().all(Result::is_ok));
    assert_eq!(count_actions(&h, |a| matches!(a, Action::Unban { .. })), 1);
}

#[tokio::test]
async fn a_permanent_ban_sets_no_timer() {
    let h = with_rules(json!([rule(
        "b",
        json!({ "type": "words", "words": ["spoiler"] }),
        json!([{ "do": "ban" }])
    )]));
    say(&h, MEMBER, "spoiler").await;
    assert_eq!(count_actions(&h, |a| matches!(a, Action::Ban { .. })), 1);
    assert_eq!(h.jobs(), 0);
}

#[tokio::test]
async fn a_punishment_the_discord_refuses_leaves_no_case() {
    let h = with_rules(json!([rule(
        "k",
        json!({ "type": "words", "words": ["spoiler"] }),
        json!([{ "do": "kick" }])
    )]));
    // The direct message goes first and fails too, so every call is refused.
    h.api()
        .fail_with(DiscordError::Forbidden("role too high".into()));
    let result = h
        .message(Message::new(MEMBER, CHANNEL).content("spoiler"))
        .await;
    assert!(result.is_err());
    assert!(
        cases_of(&h, MEMBER).await.is_empty(),
        "no case for an action that did not happen"
    );
}

// ---- edits ----

#[tokio::test]
async fn an_edited_message_is_checked_and_an_update_without_content_is_ignored() {
    let h = with_rules(json!([rule(
        "e",
        json!({ "type": "words", "words": ["spoiler"] }),
        delete()
    )]));
    h.emit(Event::MessageUpdated(update(MEMBER, "now a spoiler")))
        .await
        .unwrap();
    assert_eq!(deletions(&h), 1);
    h.emit(Event::MessageUpdated(update(MEMBER, "")))
        .await
        .unwrap();
    assert_eq!(
        deletions(&h),
        1,
        "an update with neither content nor attachments is skipped"
    );
}

// ---- dry run ----

#[tokio::test]
async fn a_dry_run_acts_on_nothing_and_posts_what_would_have_run() {
    let h = configured(json!({
        "dry_run": true,
        "rules": [rule(
            "dry",
            json!({ "type": "words", "words": ["spoiler"] }),
            json!([{ "do": "delete" }, { "do": "warn" }])
        )]
    }));
    say(&h, MEMBER, "spoiler").await;
    assert_eq!(deletions(&h), 0);
    assert_eq!(
        count_actions(&h, |a| matches!(
            a,
            Action::Ban { .. } | Action::Timeout { .. } | Action::Kick { .. }
        )),
        0
    );
    assert!(cases_of(&h, MEMBER).await.is_empty());
    let embeds = h.sent_embeds();
    assert_eq!(embeds.len(), 1, "one embed, in the log");
    let text = embed_text(&embeds[0]);
    assert!(text.contains("[dry run] automod"), "{text}");
    assert!(text.contains("dry"), "{text}");
    assert!(text.contains("delete") && text.contains("warn"), "{text}");
}

#[tokio::test]
async fn a_dry_run_without_a_log_channel_does_nothing_at_all() {
    let h = harness();
    h.set_config(json!({
        "dry_run": true,
        "rules": [rule("dry", json!({ "type": "words", "words": ["spoiler"] }), delete())]
    }));
    say(&h, MEMBER, "spoiler").await;
    assert!(h.api().actions().is_empty());
}

#[tokio::test]
async fn a_dry_run_does_not_reset_the_flood_counter() {
    let h = configured(json!({
        "dry_run": true,
        "rules": [rule("flood", json!({ "type": "flood", "messages": 2, "per_minutes": 1 }), delete())]
    }));
    for content in ["a", "b", "c"] {
        say(&h, MEMBER, content).await;
    }
    assert_eq!(
        h.sent_embeds().len(),
        2,
        "the second message and the third one would both hit"
    );
}

// ---- invalid rules ----

#[tokio::test]
async fn invalid_rules_never_run_and_the_first_rule_with_an_id_is_the_one_that_counts() {
    let h = configured(json!({
        "rules": [
            rule("ok", json!({ "type": "words", "words": ["spoiler"] }), delete()),
            { "id": "bad_type", "check": { "type": "regex" }, "actions": [{ "do": "delete" }] },
            { "id": "Bad Id", "check": { "type": "repeat", "max_run": 3 }, "actions": [{ "do": "delete" }] },
            rule("ok", json!({ "type": "words", "words": ["other"] }), delete()),
            { "id": "no_words", "check": { "type": "words", "words": [] }, "actions": [{ "do": "delete" }] },
            { "id": "no_files", "check": { "type": "attachments" }, "actions": [{ "do": "delete" }] },
            { "id": "extra", "check": { "type": "repeat", "max_run": 3, "extra": 1 }, "actions": [{ "do": "delete" }] },
            { "id": "no_actions", "check": { "type": "repeat", "max_run": 3 }, "actions": [] }
        ]
    }));
    say(&h, MEMBER, "xxxx").await;
    say(&h, MEMBER, "other").await;
    assert_eq!(
        deletions(&h),
        0,
        "only the first `ok` rule is there, and it looks for spoiler"
    );
    say(&h, MEMBER, "spoiler").await;
    assert_eq!(deletions(&h), 1);

    let view = h
        .invoke(
            Invocation::new("automod")
                .subcommand("status")
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    let text = description(&view);
    for label in [
        "bad_type",
        "Bad Id",
        "no_words",
        "no_files",
        "extra",
        "no_actions",
    ] {
        assert!(text.contains(label), "{label} is listed: {text}");
    }
}

// ---- /automod status ----

#[tokio::test]
async fn status_shows_the_dry_run_state_the_counts_and_each_rule_in_one_line() {
    let h = configured(json!({
        "rules": [
            rule("spoilers", json!({ "type": "words", "words": ["spoiler"] }), json!([{ "do": "delete" }, { "do": "warn" }])),
            scoped(
                { let mut off = rule("links", json!({ "type": "links", "mode": "any", "domains": [] }), json!([{ "do": "notify", "channel": NOTIFY.to_string() }])); off["enabled"] = json!(false); off },
                json!({})
            ),
            { "id": "broken", "check": { "type": "regex" }, "actions": [{ "do": "delete" }] }
        ]
    }));
    let replies = h
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
        text.contains("spoilers") && text.contains("words") && text.contains("delete, warn"),
        "{text}"
    );
    assert!(text.contains("broken"), "{text}");
    assert!(
        replies
            .iter()
            .all(|reply| reply.embeds.iter().all(|embed| embed.description.is_some()))
    );
    assert!(
        replies.iter().all(|reply| reply.ephemeral),
        "the status is private"
    );
}

#[tokio::test]
async fn status_needs_the_view_permission() {
    let h = configured(json!({}));
    let replies = h
        .invoke(Invocation::new("automod").subcommand("status"))
        .await
        .unwrap();
    // The refusal is the SDK's answer; the status is not in it.
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
    let h = configured(json!({
        "rules": [
            { "id": "links_1", "check": { "type": "links", "mode": "allowlist", "domains": ["example.com"] },
              "scope": { "channels": ["5"] }, "actions": [{ "do": "delete" }, { "do": "notify", "channel": "60" }] },
            { "id": "words_2", "check": { "type": "words", "words": ["bad"], "match": "whole" },
              "actions": [{ "do": "warn" }, { "do": "timeout", "duration": "1h" }, { "do": "ban", "duration": "7d", "delete_days": 1 }, { "do": "kick" }] },
            { "id": "flood_9", "check": { "type": "flood", "messages": 5, "per_minutes": 1 }, "actions": [{ "do": "timeout", "duration": "10m" }] }
        ]
    }));
    let replies = h
        .invoke(
            Invocation::new("automod")
                .subcommand("status")
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    assert!(
        description(&replies).contains("0 invalid"),
        "{}",
        description(&replies)
    );
}
