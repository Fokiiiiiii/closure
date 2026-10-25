//! The moderation module, run against the SDK's pretend Discord.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::sync::Arc;
use std::time::Duration;

use closure_config::{ConfigCompiler, parse_overrides};
use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::CommandOptionValue;
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use closure_i18n::missing_keys;
use closure_mod_moderation::{builder, module};
use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{Level, Locale};
use serde_json::json;
use twilight_model::guild::{Member, Role};
use twilight_model::id::Id;

const MODERATOR_ROLE: u64 = 10;
const ADMIN_ROLE: u64 = 20;
const GUILD: u64 = 1;
const MODERATOR: u64 = 100;
const UNPRIVILEGED: u64 = 55;
const MEMBER: u64 = 7;
const BOT: u64 = 8;
const ABOVE: u64 = 9;
const FREE: u64 = 42;
/// The owner of the server in every run whose invocation does not name the invoker as owner.
const OWNER: u64 = 999_999_999;

const DAY: u64 = 86_400;

fn member(id: u64, roles: &[u64], bot: bool) -> Member {
    serde_json::from_value(json!({
        "deaf": false,
        "mute": false,
        "flags": 0,
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "user": {
            "id": id.to_string(),
            "username": "member",
            "discriminator": "0",
            "avatar": null,
            "bot": bot,
        },
    }))
    .unwrap()
}

fn role(id: u64, position: i64) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(),
        "name": "role",
        "color": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "hoist": false,
        "managed": false,
        "mentionable": false,
        "permissions": "0",
        "position": position,
        "flags": 0,
    }))
    .unwrap()
}

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR_ROLE), Level::MODERATOR);
        permissions
            .role_levels
            .insert(Id::new(ADMIN_ROLE), Level::ADMIN);
    });
    harness.api().set_roles(
        Id::new(GUILD),
        vec![role(MODERATOR_ROLE, 5), role(ADMIN_ROLE, 9)],
    );
    for user in [MEMBER, FREE, ABOVE] {
        harness
            .api()
            .set_not_a_member(Id::new(GUILD), Id::new(user));
    }
    harness
}

fn user(id: u64) -> CommandOptionValue {
    CommandOptionValue::User(Id::new(id))
}

fn by_moderator(invocation: Invocation) -> Invocation {
    invocation.roles([MODERATOR_ROLE]).user(MODERATOR)
}

fn warn(target: u64, reason: Option<&str>) -> Invocation {
    let invocation = Invocation::new("warn").option("user", user(target));
    let invocation = match reason {
        Some(reason) => invocation.option("reason", CommandOptionValue::String(reason.to_owned())),
        None => invocation,
    };
    by_moderator(invocation)
}

fn note(target: u64, text: &str) -> Invocation {
    by_moderator(
        Invocation::new("note")
            .option("user", user(target))
            .option("text", CommandOptionValue::String(text.to_owned())),
    )
}

fn warnings(target: u64) -> Invocation {
    by_moderator(Invocation::new("warnings").option("user", user(target)))
}

fn case_view(number: i64) -> Invocation {
    by_moderator(
        Invocation::new("case")
            .subcommand("view")
            .option("number", CommandOptionValue::Integer(number)),
    )
}

fn case_list(target: u64, before: Option<i64>) -> Invocation {
    let invocation = Invocation::new("case")
        .subcommand("list")
        .option("user", user(target));
    let invocation = match before {
        Some(before) => invocation.option("before", CommandOptionValue::Integer(before)),
        None => invocation,
    };
    by_moderator(invocation)
}

fn case_revoke(number: i64, reason: Option<&str>) -> Invocation {
    let invocation = Invocation::new("case")
        .subcommand("revoke")
        .option("number", CommandOptionValue::Integer(number));
    let invocation = match reason {
        Some(reason) => invocation.option("reason", CommandOptionValue::String(reason.to_owned())),
        None => invocation,
    };
    by_moderator(invocation)
}

fn text(replies: &[Reply]) -> &str {
    replies.first().map_or("", |reply| reply.content.as_str())
}

/// The lines of the answer that start with a case number, in the order given.
fn numbered_lines(replies: &[Reply]) -> Vec<&str> {
    text(replies)
        .lines()
        .filter(|line| line.starts_with('#'))
        .collect()
}

async fn cases_of(harness: &Harness, target: u64) -> Vec<closure_sdk::Case> {
    harness
        .cases()
        .for_target(Id::new(target), None, 25)
        .await
        .unwrap()
}

// ---- /warn ----

#[tokio::test]
async fn a_moderator_warns_and_the_warning_is_a_case_that_expires_in_thirty_days() {
    let harness = harness();
    let replies = harness.invoke(warn(MEMBER, Some("spam"))).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(text(&replies).contains("Active warnings: 1"), "{replies:?}");
    assert!(replies[0].ephemeral, "answered to the moderator alone");

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "warn");
    assert_eq!(case.target, Id::new(MEMBER));
    assert_eq!(case.moderator, Id::new(MODERATOR));
    assert_eq!(case.reason.as_deref(), Some("spam"));
    assert_eq!(case.expires_at, Some(case.created_at + 30 * 86_400));
    assert!(case.revoked.is_none());
}

#[tokio::test]
async fn a_second_warning_counts_the_active_ones_and_the_cooldown_holds_the_first_back() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("a"))).await.unwrap();

    let held = harness.invoke(warn(MEMBER, Some("b"))).await.unwrap();
    assert!(text(&held).to_lowercase().contains("wait"), "{held:?}");
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);

    harness.advance(Duration::from_secs(3));
    let replies = harness.invoke(warn(MEMBER, Some("b"))).await.unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    assert!(text(&replies).contains("Active warnings: 2"), "{replies:?}");
}

#[tokio::test]
async fn the_expiry_follows_the_configured_days() {
    let harness = harness();
    harness.set_config(json!({ "warn_expire_days": 1 }));
    harness.invoke(warn(MEMBER, Some("x"))).await.unwrap();
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        case.expires_at,
        Some(case.created_at + i64::try_from(DAY).unwrap())
    );
}

#[tokio::test]
async fn a_reason_can_be_required() {
    let harness = harness();
    harness.set_config(json!({ "require_reason": true }));

    let refused = harness.invoke(warn(MEMBER, None)).await.unwrap();
    assert!(text(&refused).contains("reason"), "{refused:?}");
    assert!(cases_of(&harness, MEMBER).await.is_empty());

    harness.advance(Duration::from_secs(3));
    harness.invoke(warn(MEMBER, Some("given"))).await.unwrap();
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
}

#[tokio::test]
async fn a_moderator_cannot_warn_themselves_the_owner_a_bot_or_someone_above_them() {
    let harness = harness();
    harness
        .api()
        .set_member(Id::new(GUILD), member(BOT, &[], true));
    harness
        .api()
        .set_member(Id::new(GUILD), member(ABOVE, &[ADMIN_ROLE], false));

    for target in [MODERATOR, OWNER, BOT, ABOVE] {
        // Past the cooldown, so that the refusal is the check's and not the cooldown's.
        harness.advance(Duration::from_secs(3));
        let refused = harness.invoke(warn(target, Some("no"))).await.unwrap();
        assert!(!text(&refused).is_empty(), "{target}: {refused:?}");
        assert!(
            cases_of(&harness, target).await.is_empty(),
            "no case for {target}"
        );
    }
    assert_eq!(harness.jobs(), 0, "nothing scheduled either");
}

#[tokio::test]
async fn someone_who_is_not_a_member_can_be_warned() {
    let harness = harness();
    harness.invoke(warn(FREE, Some("left"))).await.unwrap();
    assert_eq!(cases_of(&harness, FREE).await.len(), 1);
}

#[tokio::test]
async fn without_the_permission_nothing_is_warned() {
    let harness = harness();
    let denied = harness
        .invoke(
            Invocation::new("warn")
                .option("user", user(MEMBER))
                .user(UNPRIVILEGED),
        )
        .await
        .unwrap();
    assert!(text(&denied).contains("level"), "{denied:?}");
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn the_warned_member_is_direct_messaged_in_the_server_language() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("spam"))).await.unwrap();
    let messages = dm_contents(&harness);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("spam"), "{messages:?}");
    assert!(messages[0].contains("Active warnings: 1"), "{messages:?}");

    harness.set_guild_locale(Locale::Ja);
    harness.advance(Duration::from_secs(3));
    harness.invoke(warn(MEMBER, Some("荒らし"))).await.unwrap();
    let messages = dm_contents(&harness);
    assert!(
        messages[1].contains("サーバーで警告を受けました"),
        "{messages:?}"
    );
}

#[tokio::test]
async fn the_direct_message_can_be_turned_off() {
    let harness = harness();
    harness.set_config(json!({ "dm_on_action": false }));
    let replies = harness.invoke(warn(MEMBER, Some("quiet"))).await.unwrap();
    assert!(dm_contents(&harness).is_empty());
    assert!(
        !text(&replies).contains("could not be delivered"),
        "{replies:?}"
    );
}

#[tokio::test]
async fn a_closed_direct_message_does_not_fail_the_warning() {
    let harness = harness();
    // The warning's own request is the first call; the direct message is the second.
    harness
        .api()
        .fail_nth(2, DiscordError::DirectMessagesClosed);
    let replies = harness.invoke(warn(MEMBER, Some("closed"))).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(
        text(&replies).contains("could not be delivered"),
        "{replies:?}"
    );
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
}

#[tokio::test]
async fn a_warning_is_posted_to_the_log_channel_when_there_is_one() {
    let logged = harness();
    logged.set_log_channel(50);
    logged.invoke(warn(MEMBER, Some("logged"))).await.unwrap();
    let embeds = logged.sent_embeds();
    assert_eq!(embeds.len(), 1, "{embeds:?}");
    assert_eq!(embeds[0].title.as_deref(), Some("Case #1 · Warning"));

    let quiet = harness();
    quiet.invoke(warn(MEMBER, Some("unlogged"))).await.unwrap();
    assert!(quiet.sent_embeds().is_empty());
    assert_eq!(cases_of(&quiet, MEMBER).await.len(), 1);
}

// ---- /warnings ----

#[tokio::test]
async fn warnings_list_only_the_active_ones_newest_first() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("a"))).await.unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(warn(MEMBER, Some("b"))).await.unwrap();
    harness.invoke(note(MEMBER, "a memo")).await.unwrap();

    let replies = harness.invoke(warnings(MEMBER)).await.unwrap();
    let lines = numbered_lines(&replies);
    assert_eq!(lines.len(), 2, "{replies:?}");
    assert!(lines[0].starts_with("#2 b"), "{lines:?}");
    assert!(lines[1].starts_with("#1 a"), "{lines:?}");
    assert!(!text(&replies).contains("a memo"), "{replies:?}");
    assert!(text(&replies).contains("(2)"), "{replies:?}");
}

#[tokio::test]
async fn warnings_of_someone_without_any_say_so() {
    let harness = harness();
    let replies = harness.invoke(warnings(MEMBER)).await.unwrap();
    assert!(text(&replies).contains("no active warnings"), "{replies:?}");

    let ja = harness
        .invoke(warnings(MEMBER).locale(Locale::Ja))
        .await
        .unwrap();
    assert!(text(&ja).contains("有効な警告はありません"), "{ja:?}");
}

#[tokio::test]
async fn an_expired_or_revoked_warning_is_not_listed() {
    let harness = harness();
    harness.set_config(json!({ "warn_expire_days": 1 }));
    harness.invoke(warn(MEMBER, Some("old"))).await.unwrap();
    harness.advance(Duration::from_secs(2 * DAY));
    harness.invoke(warn(MEMBER, Some("new"))).await.unwrap();

    let replies = harness.invoke(warnings(MEMBER)).await.unwrap();
    let lines = numbered_lines(&replies);
    assert_eq!(lines.len(), 1, "{replies:?}");
    assert!(lines[0].starts_with("#2 new"), "{lines:?}");

    harness
        .invoke(case_revoke(2, Some("mistake")))
        .await
        .unwrap();
    let replies = harness.invoke(warnings(MEMBER)).await.unwrap();
    assert!(text(&replies).contains("no active warnings"), "{replies:?}");
}

#[tokio::test]
async fn at_most_ten_of_the_newest_warnings_are_listed() {
    let harness = harness();
    for n in 1..=11 {
        harness
            .invoke(warn(MEMBER, Some(&format!("w{n}"))))
            .await
            .unwrap();
        harness.advance(Duration::from_secs(3));
    }
    let replies = harness.invoke(warnings(MEMBER)).await.unwrap();
    let lines = numbered_lines(&replies);
    assert_eq!(lines.len(), 10, "{replies:?}");
    assert!(lines[0].starts_with("#11 "), "{lines:?}");
    assert!(lines[9].starts_with("#2 "), "{lines:?}");
    assert!(
        !lines.iter().any(|line| line.starts_with("#1 ")),
        "{lines:?}"
    );
}

// ---- /note ----

#[tokio::test]
async fn a_note_is_a_case_without_expiry_and_no_warning() {
    let harness = harness();
    let replies = harness.invoke(note(MEMBER, "memo")).await.unwrap();
    assert!(replies[0].ephemeral, "{replies:?}");

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "note");
    assert_eq!(case.expires_at, None);
    assert_eq!(case.reason.as_deref(), Some("memo"));
    assert_eq!(case.target, Id::new(MEMBER));

    let listed = harness.invoke(warnings(MEMBER)).await.unwrap();
    assert!(text(&listed).contains("no active warnings"), "{listed:?}");
}

#[tokio::test]
async fn a_note_may_be_about_the_moderator_or_the_owner() {
    let harness = harness();
    harness.invoke(note(MODERATOR, "self")).await.unwrap();
    harness.invoke(note(OWNER, "owner")).await.unwrap();
    assert_eq!(cases_of(&harness, MODERATOR).await.len(), 1);
    assert_eq!(cases_of(&harness, OWNER).await.len(), 1);
}

// ---- /case ----

#[tokio::test]
async fn a_case_is_shown_as_the_log_shows_it() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("shown"))).await.unwrap();

    let replies = harness.invoke(case_view(1)).await.unwrap();
    assert!(replies[0].ephemeral, "{replies:?}");
    assert_eq!(replies[0].embeds.len(), 1, "{replies:?}");
    assert_eq!(
        replies[0].embeds[0].title.as_deref(),
        Some("Case #1 · Warning")
    );

    let missing = harness.invoke(case_view(99)).await.unwrap();
    assert!(text(&missing).contains("#99"), "{missing:?}");
}

#[tokio::test]
async fn case_list_shows_kind_state_and_reason_and_pages_back_with_before() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("a"))).await.unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(warn(MEMBER, Some("b"))).await.unwrap();
    harness.invoke(note(MEMBER, "c")).await.unwrap();

    let replies = harness.invoke(case_list(MEMBER, None)).await.unwrap();
    let lines = numbered_lines(&replies);
    assert_eq!(lines.len(), 3, "{replies:?}");
    assert_eq!(lines[0], "#3 note active c", "{lines:?}");
    assert_eq!(lines[1], "#2 warn active b", "{lines:?}");
    assert_eq!(lines[2], "#1 warn active a", "{lines:?}");

    harness.invoke(case_revoke(2, None)).await.unwrap();
    harness.advance(Duration::from_secs(31 * DAY));
    let replies = harness.invoke(case_list(MEMBER, Some(3))).await.unwrap();
    let lines = numbered_lines(&replies);
    assert_eq!(
        lines,
        ["#2 warn revoked b", "#1 warn expired a"],
        "{replies:?}"
    );
}

#[tokio::test]
async fn an_empty_case_list_says_so() {
    let harness = harness();
    let replies = harness.invoke(case_list(MEMBER, None)).await.unwrap();
    assert!(text(&replies).contains("has no cases"), "{replies:?}");
}

#[tokio::test]
async fn revoking_a_case_takes_it_out_once_and_logs_it_grey() {
    let harness = harness();
    harness.set_log_channel(50);
    harness.invoke(warn(MEMBER, Some("x"))).await.unwrap();

    let replies = harness
        .invoke(case_revoke(1, Some("mistake")))
        .await
        .unwrap();
    assert!(
        text(&replies).contains("Case #1 was revoked"),
        "{replies:?}"
    );
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        case.revoked.as_ref().and_then(|r| r.reason.as_deref()),
        Some("mistake")
    );

    let again = harness.invoke(case_revoke(1, None)).await.unwrap();
    assert!(text(&again).contains("already revoked"), "{again:?}");
    let missing = harness.invoke(case_revoke(99, None)).await.unwrap();
    assert!(text(&missing).contains("#99"), "{missing:?}");

    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), 2, "the warning and its revocation only");
    assert_eq!(embeds[1].color, Some(0x0095_A5A6), "grey when revoked");
    let listed = harness.invoke(warnings(MEMBER)).await.unwrap();
    assert!(text(&listed).contains("no active warnings"), "{listed:?}");
}

#[tokio::test]
async fn the_case_commands_need_their_permissions() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("x"))).await.unwrap();

    let view = harness
        .invoke(
            Invocation::new("case")
                .subcommand("view")
                .option("number", CommandOptionValue::Integer(1))
                .user(UNPRIVILEGED),
        )
        .await
        .unwrap();
    assert!(text(&view).contains("level"), "{view:?}");

    let revoke = harness
        .invoke(
            Invocation::new("case")
                .subcommand("revoke")
                .option("number", CommandOptionValue::Integer(1))
                .user(UNPRIVILEGED),
        )
        .await
        .unwrap();
    assert!(text(&revoke).contains("level"), "{revoke:?}");
    assert!(
        harness
            .cases()
            .get(1)
            .await
            .unwrap()
            .unwrap()
            .revoked
            .is_none()
    );
}

// ---- settings and texts ----

#[test]
fn the_settings_default_to_thirty_days_a_direct_message_and_no_required_reason() {
    let manifest = module().unwrap().manifest().clone();
    assert_eq!(
        manifest.default_config,
        json!({ "warn_expire_days": 30, "dm_on_action": true, "require_reason": false })
    );
}

#[test]
fn the_settings_are_checked_against_their_schema() {
    let registry = Arc::new(
        ModuleRegistry::new(vec![Arc::new(module().unwrap()) as Arc<dyn Module>]).unwrap(),
    );
    let compiler = ConfigCompiler::new(registry).unwrap();
    let accepted = |config: serde_json::Value| {
        let yaml = format!(
            "modules:\n  moderation:\n    config: {}\n",
            serde_json::to_string(&config).unwrap()
        );
        compiler.compile(&parse_overrides(&yaml).unwrap()).is_ok()
    };
    assert!(accepted(json!({ "warn_expire_days": 1 })));
    assert!(accepted(json!({ "warn_expire_days": 365 })));
    assert!(!accepted(json!({ "warn_expire_days": 0 })));
    assert!(!accepted(json!({ "warn_expire_days": 366 })));
    assert!(!accepted(json!({ "dm_on_action": "yes" })));
    assert!(!accepted(json!({ "expire": 3 })));
}

#[test]
fn every_text_is_in_both_languages() {
    let en = include_str!("../locales/en.ftl");
    let ja = include_str!("../locales/ja.ftl");
    assert_eq!(missing_keys(en, ja).unwrap(), Vec::<String>::new());
    assert_eq!(missing_keys(ja, en).unwrap(), Vec::<String>::new());
}

fn dm_contents(harness: &Harness) -> Vec<String> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendDirectMessage { content, .. } => content,
            _ => None,
        })
        .collect()
}
