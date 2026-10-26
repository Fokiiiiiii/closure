//! The moderation module, run against the SDK's pretend Discord.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use closure_config::{ConfigCompiler, parse_overrides};
use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::{CommandOptionValue, MemberInfo, UserInfo};
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use closure_i18n::missing_keys;
use closure_mod_moderation::{builder, module};
use closure_sdk::testing::{Click, Harness, Invocation, Reply, ReplyKind};
use closure_sdk::{CaseKind, Event, EventKind, Level, Locale, ModuleError, NewCase};
use serde_json::json;
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::SelectMenu;
use twilight_model::gateway::Intents;
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
    assert!(text(&replies).contains("1 active."), "{replies:?}");
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
    assert!(text(&replies).contains("2 active."), "{replies:?}");
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

// ---- tones ----

#[tokio::test]
async fn a_confirmed_punishment_is_a_success_with_the_guild_emoji_and_a_list_stays_info() {
    const SUCCESS_COLOUR: u32 = 0x0057_F287;
    const INFO_COLOUR: u32 = 0x0058_65F2;
    let harness = harness();
    let warned = harness.invoke(warn(MEMBER, Some("tone"))).await.unwrap();
    assert_eq!(
        warned[0].embeds[0].color,
        Some(SUCCESS_COLOUR),
        "{warned:?}"
    );
    assert!(
        warned[0].embeds[0]
            .description
            .as_deref()
            .is_some_and(|text| text.starts_with("✅ ")),
        "{warned:?}"
    );
    let timed = harness
        .invoke(timeout_of(MEMBER, Some("1h"), Some("tone")))
        .await
        .unwrap();
    assert_eq!(timed[0].embeds[0].color, Some(SUCCESS_COLOUR), "{timed:?}");
    let listed = harness.invoke(warnings(MEMBER)).await.unwrap();
    assert_eq!(listed[0].embeds[0].color, Some(INFO_COLOUR), "{listed:?}");
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
        json!({ "warn_expire_days": 30, "dm_on_action": true, "require_reason": false, "ban_delete_days": 0, "escalation": [], "timeout_method": "discord", "timeout_role": null, "voice_mute_method": "discord", "voice_mute_role": null, "templates": {} })
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
    assert!(accepted(json!({ "ban_delete_days": 7 })));
    assert!(!accepted(json!({ "ban_delete_days": 8 })));
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

// ---- /timeout, /untimeout, /kick, /ban, /unban ----

fn by_admin(invocation: Invocation) -> Invocation {
    invocation.roles([ADMIN_ROLE]).user(MODERATOR)
}

fn with_reason(invocation: Invocation, reason: Option<&str>) -> Invocation {
    match reason {
        Some(reason) => invocation.option("reason", CommandOptionValue::String(reason.to_owned())),
        None => invocation,
    }
}

fn timeout(target: u64, duration: &str, reason: Option<&str>) -> Invocation {
    timeout_of(target, Some(duration), reason)
}

/// A `/timeout` whose duration may be left out.
fn timeout_of(target: u64, duration: Option<&str>, reason: Option<&str>) -> Invocation {
    let invocation = Invocation::new("timeout").option("user", user(target));
    let invocation = match duration {
        Some(duration) => {
            invocation.option("duration", CommandOptionValue::String(duration.to_owned()))
        }
        None => invocation,
    };
    by_moderator(with_reason(invocation, reason))
}

fn untimeout(target: u64, reason: Option<&str>) -> Invocation {
    by_moderator(with_reason(
        Invocation::new("untimeout").option("user", user(target)),
        reason,
    ))
}

fn kick(target: u64, reason: Option<&str>) -> Invocation {
    by_moderator(with_reason(
        Invocation::new("kick").option("user", user(target)),
        reason,
    ))
}

fn ban(
    target: u64,
    reason: Option<&str>,
    duration: Option<&str>,
    delete_days: Option<i64>,
) -> Invocation {
    let invocation = with_reason(Invocation::new("ban").option("user", user(target)), reason);
    let invocation = match duration {
        Some(duration) => {
            invocation.option("duration", CommandOptionValue::String(duration.to_owned()))
        }
        None => invocation,
    };
    let invocation = match delete_days {
        Some(days) => invocation.option("delete_days", CommandOptionValue::Integer(days)),
        None => invocation,
    };
    by_admin(invocation)
}

fn unban(target: u64, reason: Option<&str>) -> Invocation {
    by_admin(with_reason(
        Invocation::new("unban").option("user", user(target)),
        reason,
    ))
}

/// What the module did to Discord, in order: the direct messages and the timeouts, kicks, bans
/// and unbans. Lookups of members are left out.
fn sequence(harness: &Harness) -> Vec<&'static str> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SendDirectMessage { .. } => Some("dm"),
            Action::Timeout {
                duration_seconds: Some(_),
                ..
            } => Some("timeout"),
            Action::Timeout {
                duration_seconds: None,
                ..
            } => Some("untimeout"),
            Action::Kick { .. } => Some("kick"),
            Action::Ban { .. } => Some("ban"),
            Action::Unban { .. } => Some("unban"),
            Action::AddRole { .. } => Some("role+"),
            Action::RemoveRole { .. } => Some("role-"),
            Action::SetVoiceMute { mute: true, .. } => Some("vcmute"),
            Action::SetVoiceMute { mute: false, .. } => Some("vcunmute"),
            _ => None,
        })
        .collect()
}

fn ban_deletions(harness: &Harness) -> Vec<u32> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::Ban {
                delete_message_seconds,
                ..
            } => Some(*delete_message_seconds),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_timeout_is_applied_before_the_direct_message_and_expires_with_its_case() {
    let harness = harness();
    let replies = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(text(&replies).contains("1 hour"), "{replies:?}");
    assert_eq!(sequence(&harness), ["timeout", "dm"], "{replies:?}");
    assert!(harness.api().actions().iter().any(|action| matches!(
        action,
        Action::Timeout {
            duration_seconds: Some(3_600),
            ..
        }
    )));

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "timeout");
    assert_eq!(case.target, Id::new(MEMBER));
    assert_eq!(case.reason.as_deref(), Some("spam"));
    assert_eq!(case.expires_at, Some(case.created_at + 3_600));
    assert!(dm_contents(&harness)[0].contains("spam"));
}

#[tokio::test]
async fn a_timeout_lasts_at_most_twenty_eight_days() {
    let harness = harness();
    harness
        .invoke(timeout(MEMBER, "28d", Some("spam")))
        .await
        .unwrap();
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);

    harness.advance(Duration::from_secs(3));
    let refused = harness
        .invoke(timeout(MEMBER, "2419201s", None))
        .await
        .unwrap();
    assert!(text(&refused).contains("28 days"), "{refused:?}");
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
    assert_eq!(sequence(&harness), ["timeout", "dm"]);
}

#[tokio::test]
async fn a_timeout_refuses_the_moderator_the_owner_a_bot_or_someone_above_them() {
    let harness = harness();
    harness
        .api()
        .set_member(Id::new(GUILD), member(BOT, &[], true));
    harness
        .api()
        .set_member(Id::new(GUILD), member(ABOVE, &[ADMIN_ROLE], false));

    for target in [MODERATOR, OWNER, BOT, ABOVE] {
        harness.advance(Duration::from_secs(3));
        let refused = harness
            .invoke(timeout(target, "1h", Some("no")))
            .await
            .unwrap();
        assert!(!text(&refused).is_empty(), "{target}: {refused:?}");
        assert!(
            cases_of(&harness, target).await.is_empty(),
            "no case for {target}"
        );
    }
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
}

#[tokio::test]
async fn a_timeout_needs_a_duration_it_can_read() {
    let harness = harness();
    let replies = harness
        .invoke(timeout(MEMBER, "banana", None))
        .await
        .unwrap();
    assert!(!text(&replies).is_empty(), "{replies:?}");
    assert!(sequence(&harness).is_empty());
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn a_timeout_needs_the_timeout_permission() {
    let harness = harness();
    let denied = harness
        .invoke(
            Invocation::new("timeout")
                .option("user", user(MEMBER))
                .option("duration", CommandOptionValue::String("1h".to_owned()))
                .user(UNPRIVILEGED),
        )
        .await
        .unwrap();
    assert!(text(&denied).contains("level"), "{denied:?}");
    assert!(sequence(&harness).is_empty());
}

#[tokio::test]
async fn an_untimeout_lifts_the_timeout_and_revokes_the_one_it_lifts() {
    let harness = harness();
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(untimeout(MEMBER, Some("sorry")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    assert_eq!(sequence(&harness), ["timeout", "dm", "untimeout"]);

    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "untimeout");
    let timed = harness.cases().get(1).await.unwrap().unwrap();
    let revoked = timed.revoked.expect("the timeout is revoked");
    assert_eq!(revoked.reason.as_deref(), Some("removed by case #2"));
}

#[tokio::test]
async fn an_untimeout_without_a_timeout_still_succeeds() {
    let harness = harness();
    let replies = harness.invoke(untimeout(MEMBER, None)).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["untimeout"]);
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
}

#[tokio::test]
async fn a_kick_is_messaged_first_then_kicked_and_recorded() {
    let harness = harness();
    harness.set_log_channel(50);
    let replies = harness.invoke(kick(MEMBER, Some("rude"))).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["dm", "kick"]);

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "kick");
    assert_eq!(case.expires_at, None);
    assert!(dm_contents(&harness)[0].contains("rude"));
    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), 1, "{embeds:?}");
    assert_eq!(embeds[0].title.as_deref(), Some("Case #1 · Kick"));
}

#[tokio::test]
async fn a_kick_without_direct_messages_is_only_the_kick() {
    let harness = harness();
    harness.set_config(json!({ "dm_on_action": false }));
    harness.invoke(kick(MEMBER, Some("rude"))).await.unwrap();
    assert_eq!(sequence(&harness), ["kick"]);
}

#[tokio::test]
async fn a_kick_the_discord_refuses_leaves_no_case_and_no_log() {
    let harness = harness();
    harness.set_log_channel(50);
    // The lookup of the member is the first call, the direct message the second, the kick the third.
    harness
        .api()
        .fail_nth(3, DiscordError::Forbidden("missing permission".to_owned()));
    assert!(harness.invoke(kick(MEMBER, Some("rude"))).await.is_err());
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert!(harness.sent_embeds().is_empty());
}

#[tokio::test]
async fn a_permanent_ban_deletes_no_messages_by_default_and_schedules_nothing() {
    let harness = harness();
    let replies = harness
        .invoke(ban(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["dm", "ban"]);
    assert_eq!(ban_deletions(&harness), [0]);
    assert_eq!(harness.jobs(), 0);

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "ban");
    assert_eq!(case.expires_at, None);
    let banned = harness
        .api()
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::Ban { reason, .. } => Some(reason.as_str().to_owned()),
            _ => None,
        })
        .unwrap();
    assert!(banned.contains("raid"), "{banned}");
}

#[tokio::test]
async fn a_ban_deletes_the_days_of_the_server_setting_or_the_ones_given() {
    let harness = harness();
    harness.set_config(json!({ "ban_delete_days": 2 }));
    harness
        .invoke(ban(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban(FREE, Some("raid"), None, Some(1)))
        .await
        .unwrap();
    assert_eq!(ban_deletions(&harness), [172_800, 86_400]);
}

#[tokio::test]
async fn a_ban_deletes_at_most_seven_days_of_messages() {
    let harness = harness();
    let refused = harness
        .invoke(ban(MEMBER, None, None, Some(8)))
        .await
        .unwrap();
    assert!(!text(&refused).contains("case #"), "{refused:?}");
    assert!(sequence(&harness).is_empty());
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn only_an_admin_bans_or_unbans() {
    let harness = harness();
    let denied = harness
        .invoke(
            Invocation::new("ban")
                .option("user", user(MEMBER))
                .user(MODERATOR)
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    assert!(text(&denied).contains("level"), "{denied:?}");
    let denied = harness
        .invoke(
            Invocation::new("unban")
                .option("user", user(MEMBER))
                .user(MODERATOR)
                .roles([MODERATOR_ROLE]),
        )
        .await
        .unwrap();
    assert!(text(&denied).contains("level"), "{denied:?}");
    assert!(sequence(&harness).is_empty());
}

#[tokio::test]
async fn a_timed_ban_is_lifted_when_its_time_is_up() {
    let harness = harness();
    let replies = harness
        .invoke(ban(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    assert!(text(&replies).contains("1 day"), "{replies:?}");
    assert_eq!(harness.jobs(), 1);

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        case.expires_at,
        Some(case.created_at + i64::try_from(DAY).unwrap())
    );

    harness.advance(Duration::from_secs(DAY - 1));
    assert!(harness.run_due_jobs().await.is_empty(), "not due yet");
    harness.advance(Duration::from_secs(1));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["dm", "ban", "unban"]);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_timed_ban_lifted_by_hand_is_not_an_error_for_its_lift() {
    let harness = harness();
    harness
        .invoke(ban(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(DAY));
    harness.api().fail_nth(1, DiscordError::NotFound);
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_lift_that_fails_otherwise_is_left_to_run_again() {
    let harness = harness();
    harness
        .invoke(ban(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(DAY));
    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("missing permission".to_owned()));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_err(), "{ended:?}");
    assert_eq!(harness.jobs(), 1, "still waiting to run again");
}

#[tokio::test]
async fn a_ban_the_discord_refuses_leaves_no_case_and_no_timer() {
    let harness = harness();
    // The lookup of the member, the direct message, then the ban.
    harness
        .api()
        .fail_nth(3, DiscordError::Forbidden("missing permission".to_owned()));
    assert!(
        harness
            .invoke(ban(MEMBER, Some("raid"), Some("1d"), None))
            .await
            .is_err()
    );
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_timed_ban_holds_even_when_its_timer_cannot_be_set() {
    let harness = harness();
    harness.set_log_channel(50);
    // A server may hold 10 000 jobs at most; fill it up so that the timer is refused.
    for n in 0..10_000_u32 {
        harness
            .scheduler()
            .job("filler", &n)
            .unwrap()
            .after(Duration::from_secs(60))
            .await
            .unwrap();
    }
    let replies = harness
        .invoke(ban(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(text(&replies).contains("timer"), "{replies:?}");
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
    assert_eq!(harness.sent_embeds().len(), 1);
    assert_eq!(harness.jobs(), 10_000);
}

#[tokio::test]
async fn a_ban_for_good_that_replaces_a_timed_one_drops_its_timer() {
    let harness = harness();
    harness
        .invoke(ban(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    // The old timer would lift the new ban when its time is up.
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn an_unban_lifts_the_ban_cancels_its_timer_and_revokes_the_ban() {
    let harness = harness();
    harness
        .invoke(ban(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 1);
    harness.advance(Duration::from_secs(3));
    let replies = harness.invoke(unban(MEMBER, Some("appeal"))).await.unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    assert_eq!(sequence(&harness), ["dm", "ban", "unban"]);
    assert_eq!(harness.jobs(), 0, "the timer is cancelled");

    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "unban");
    let banned = harness.cases().get(1).await.unwrap().unwrap();
    let revoked = banned.revoked.expect("the ban is revoked");
    assert_eq!(revoked.reason.as_deref(), Some("unbanned by case #2"));
}

#[tokio::test]
async fn an_unban_without_a_ban_still_succeeds() {
    let harness = harness();
    let replies = harness.invoke(unban(MEMBER, None)).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["unban"]);
}

#[tokio::test]
async fn every_action_is_posted_to_the_log_under_its_kind() {
    let harness = harness();
    harness.set_log_channel(50);
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(untimeout(MEMBER, None)).await.unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(kick(MEMBER, Some("rude"))).await.unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(unban(MEMBER, None)).await.unwrap();

    let embeds = harness.sent_embeds();
    let titles: Vec<&str> = embeds
        .iter()
        .map(|embed| embed.title.as_deref().unwrap_or_default())
        .collect();
    assert_eq!(
        titles,
        [
            "Case #1 · Timeout",
            "Case #2 · Timeout removed",
            "Case #3 · Kick",
            "Case #4 · Ban",
            "Case #5 · Unban",
        ]
    );
    assert_eq!(embeds[1].color, Some(0x0057_F287), "the lift is green");
}

#[tokio::test]
async fn the_actions_work_without_a_log_channel() {
    let harness = harness();
    harness.invoke(kick(MEMBER, Some("rude"))).await.unwrap();
    harness
        .invoke(ban(FREE, Some("raid"), None, None))
        .await
        .unwrap();
    assert!(harness.sent_embeds().is_empty());
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
    assert_eq!(cases_of(&harness, FREE).await.len(), 1);
}

#[tokio::test]
async fn the_direct_message_and_the_log_speak_the_server_language() {
    let harness = harness();
    harness.set_log_channel(50);
    harness.set_guild_locale(Locale::Ja);
    harness.invoke(kick(MEMBER, Some("荒らし"))).await.unwrap();
    let messages = dm_contents(&harness);
    assert!(
        messages[0].contains("サーバーからキックされました"),
        "{messages:?}"
    );
    assert!(messages[0].contains("荒らし"), "{messages:?}");
    let title = harness.sent_embeds()[0].title.clone().unwrap_or_default();
    assert!(title.contains("キック"), "{title}");
}

#[tokio::test]
async fn a_closed_direct_message_does_not_fail_a_ban() {
    let harness = harness();
    // The lookup of the member is the first call, the direct message the second.
    harness
        .api()
        .fail_nth(2, DiscordError::DirectMessagesClosed);
    let replies = harness
        .invoke(ban(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(
        text(&replies).contains("could not be delivered"),
        "{replies:?}"
    );
    assert_eq!(sequence(&harness), ["ban"]);
}

#[tokio::test]
async fn the_message_to_the_member_names_the_time_in_the_language_of_the_server() {
    let harness = harness();
    // The server speaks English; the moderator reads Japanese.
    let replies = harness
        .invoke(ban(MEMBER, Some("raid"), Some("1h"), None).locale(Locale::Ja))
        .await
        .unwrap();
    assert!(text(&replies).contains("1時間"), "{replies:?}");
    let direct = dm_contents(&harness);
    assert_eq!(direct.len(), 1, "{direct:?}");
    assert!(direct[0].contains("1 hour"), "{direct:?}");
    assert!(!direct[0].contains("1時間"), "{direct:?}");

    harness.advance(Duration::from_secs(5));
    harness
        .invoke(by_admin(timeout(MEMBER, "2h", Some("spam"))).locale(Locale::Ja))
        .await
        .unwrap();
    let direct = dm_contents(&harness);
    assert!(direct[1].contains("2 hours"), "{direct:?}");
}

// ---- escalation ----

/// Warns `target` `times` times in a row, letting the cooldown pass before each. Returns the
/// replies to the last one.
async fn warn_times(harness: &Harness, target: u64, times: usize) -> Vec<Reply> {
    let mut replies = Vec::new();
    for _ in 0..times {
        harness.advance(Duration::from_secs(3));
        replies = harness.invoke(warn(target, Some("spam"))).await.unwrap();
    }
    replies
}

async fn cases_of_kind(harness: &Harness, target: u64, kind: &str) -> usize {
    cases_of(harness, target)
        .await
        .iter()
        .filter(|case| case.kind.as_str() == kind)
        .count()
}

#[tokio::test]
async fn the_third_warning_times_the_member_out_once_and_says_so() {
    let harness = harness();
    harness.set_log_channel(50);
    harness.set_config(json!({
        "escalation": [{ "warns": 3, "action": "timeout", "duration": "1h" }]
    }));

    let second = warn_times(&harness, MEMBER, 2).await;
    assert!(!text(&second).contains("Escalation"), "{second:?}");
    assert_eq!(harness.sent_embeds().len(), 2);

    let third = warn_times(&harness, MEMBER, 1).await;
    assert!(text(&third).contains("case #3"), "{third:?}");
    assert!(
        text(&third).contains("Escalation: timed out for 1 hour — case #4."),
        "{third:?}"
    );
    assert_eq!(
        sequence(&harness),
        ["dm", "dm", "dm", "timeout", "dm"],
        "{third:?}"
    );

    let case = harness.cases().get(4).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "timeout");
    assert_eq!(case.target, Id::new(MEMBER));
    assert_eq!(
        case.moderator,
        Id::new(MODERATOR),
        "the warning's moderator"
    );
    assert_eq!(case.expires_at, Some(case.created_at + 3_600));
    assert_eq!(
        case.reason.as_deref(),
        Some("Automatic: 3 active warnings (case #3)")
    );
    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), 4, "{embeds:?}");
    assert_eq!(embeds[2].title.as_deref(), Some("Case #3 · Warning"));
    assert_eq!(embeds[3].title.as_deref(), Some("Case #4 · Timeout"));

    // Exactly three: a fourth warning does not time the member out again.
    let fourth = warn_times(&harness, MEMBER, 1).await;
    assert!(!text(&fourth).contains("Escalation"), "{fourth:?}");
    assert_eq!(
        sequence(&harness),
        ["dm", "dm", "dm", "timeout", "dm", "dm"]
    );
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 5);
}

#[tokio::test]
async fn an_escalated_kick_or_ban_is_messaged_first_and_a_timeout_after() {
    let rules = [
        (
            json!({ "warns": 1, "action": "kick" }),
            ["dm", "dm", "kick"],
        ),
        (json!({ "warns": 1, "action": "ban" }), ["dm", "dm", "ban"]),
        (
            json!({ "warns": 1, "action": "timeout", "duration": "1h" }),
            ["dm", "timeout", "dm"],
        ),
    ];
    for (rule, expected) in rules {
        let harness = harness();
        harness.set_config(json!({ "escalation": [rule] }));
        harness.invoke(warn(MEMBER, Some("x"))).await.unwrap();
        assert_eq!(sequence(&harness), expected);
    }
}

#[tokio::test]
async fn a_kick_escalation_kicks_and_records_a_kick_with_an_automatic_reason() {
    let harness = harness();
    harness.set_log_channel(50);
    harness.set_config(json!({ "escalation": [{ "warns": 1, "action": "kick" }] }));
    let replies = harness.invoke(warn(MEMBER, Some("spam"))).await.unwrap();
    assert!(
        text(&replies).contains("Escalation: kicked — case #2."),
        "{replies:?}"
    );
    let case = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "kick");
    assert_eq!(case.moderator, Id::new(MODERATOR));
    assert_eq!(case.expires_at, None);
    assert_eq!(
        case.reason.as_deref(),
        Some("Automatic: 1 active warning (case #1)")
    );
    assert_eq!(
        harness.sent_embeds()[1].title.as_deref(),
        Some("Case #2 · Kick")
    );
}

#[tokio::test]
async fn a_ban_escalation_bans_for_good_or_sets_a_timer_for_a_timed_one() {
    let forever = harness();
    forever.set_config(json!({ "escalation": [{ "warns": 1, "action": "ban" }] }));
    let replies = forever.invoke(warn(MEMBER, Some("raid"))).await.unwrap();
    assert!(
        text(&replies).contains("Escalation: banned — case #2."),
        "{replies:?}"
    );
    assert_eq!(sequence(&forever), ["dm", "dm", "ban"]);
    assert_eq!(forever.jobs(), 0, "a ban for good has no timer");

    let timed = harness();
    timed.set_config(json!({
        "escalation": [{ "warns": 1, "action": "ban", "duration": "1d" }]
    }));
    let replies = timed.invoke(warn(MEMBER, Some("raid"))).await.unwrap();
    assert!(
        text(&replies).contains("Escalation: banned for 1 day — case #2."),
        "{replies:?}"
    );
    assert_eq!(timed.jobs(), 1);

    timed.advance(Duration::from_secs(DAY));
    let ended = timed.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&timed), ["dm", "dm", "ban", "unban"]);
}

#[tokio::test]
async fn a_timeout_rule_that_is_too_long_or_unreadable_is_skipped() {
    let rules = [
        json!({ "warns": 1, "action": "timeout", "duration": "29d" }),
        json!({ "warns": 1, "action": "timeout" }),
        json!({ "warns": 1, "action": "timeout", "duration": "banana" }),
    ];
    for rule in rules {
        let harness = harness();
        harness.set_config(json!({ "escalation": [rule] }));
        let replies = harness.invoke(warn(MEMBER, Some("x"))).await.unwrap();
        assert!(text(&replies).contains("case #1"), "{replies:?}");
        assert!(
            text(&replies).contains("is invalid; skipped"),
            "{replies:?}"
        );
        assert!(!text(&replies).contains("Escalation:"), "{replies:?}");
        assert_eq!(sequence(&harness), ["dm"], "nothing is done");
        assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
    }
}

#[tokio::test]
async fn a_failed_escalation_leaves_the_warning_and_no_escalation_case() {
    let harness = harness();
    harness.set_log_channel(50);
    harness.set_config(json!({
        "escalation": [{ "warns": 3, "action": "timeout", "duration": "1h" }]
    }));
    warn_times(&harness, MEMBER, 2).await;
    // Counted from here: the lookup of the member, the log of the warning, the warning's direct
    // message, then the timeout.
    harness
        .api()
        .fail_nth(4, DiscordError::Forbidden("missing permission".to_owned()));
    let replies = warn_times(&harness, MEMBER, 1).await;
    assert!(text(&replies).contains("case #3"), "{replies:?}");
    assert!(
        text(&replies).contains("Escalation to timed out for 1 hour failed; the warning stands."),
        "{replies:?}"
    );
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 3);
    assert_eq!(sequence(&harness), ["dm", "dm", "dm"]);
    assert_eq!(harness.sent_embeds().len(), 3, "the warnings, no timeout");
}

#[tokio::test]
async fn two_rules_for_different_counts_each_apply_at_their_count() {
    let harness = harness();
    harness.set_config(json!({
        "escalation": [
            { "warns": 3, "action": "timeout", "duration": "1h" },
            { "warns": 5, "action": "ban", "duration": "7d" }
        ]
    }));
    warn_times(&harness, MEMBER, 5).await;
    assert_eq!(
        sequence(&harness),
        ["dm", "dm", "dm", "timeout", "dm", "dm", "dm", "dm", "ban"]
    );
    assert_eq!(harness.jobs(), 1, "the timer of the seven day ban");
}

#[tokio::test]
async fn of_two_rules_for_the_same_count_only_the_first_applies() {
    let harness = harness();
    harness.set_config(json!({
        "escalation": [
            { "warns": 1, "action": "kick" },
            { "warns": 1, "action": "ban" }
        ]
    }));
    harness.invoke(warn(MEMBER, Some("x"))).await.unwrap();
    assert_eq!(sequence(&harness), ["dm", "dm", "kick"]);
}

#[tokio::test]
async fn without_rules_no_warning_escalates() {
    let harness = harness();
    let replies = warn_times(&harness, MEMBER, 5).await;
    assert!(!text(&replies).contains("Escalation"), "{replies:?}");
    assert_eq!(sequence(&harness), ["dm"; 5]);
}

#[tokio::test]
async fn an_escalation_runs_without_the_direct_messages() {
    let harness = harness();
    harness.set_config(json!({
        "dm_on_action": false,
        "escalation": [{ "warns": 1, "action": "kick" }]
    }));
    let replies = harness.invoke(warn(MEMBER, Some("x"))).await.unwrap();
    assert!(text(&replies).contains("Escalation: kicked"), "{replies:?}");
    assert_eq!(sequence(&harness), ["kick"]);
}

#[tokio::test]
async fn once_the_warnings_expire_the_same_count_escalates_again() {
    let harness = harness();
    harness.set_config(json!({
        "warn_expire_days": 1,
        "escalation": [{ "warns": 3, "action": "timeout", "duration": "1h" }]
    }));
    warn_times(&harness, MEMBER, 3).await;
    assert_eq!(cases_of_kind(&harness, MEMBER, "timeout").await, 1);

    harness.advance(Duration::from_secs(2 * DAY));
    let two = warn_times(&harness, MEMBER, 2).await;
    assert!(!text(&two).contains("Escalation"), "{two:?}");
    let three = warn_times(&harness, MEMBER, 1).await;
    assert!(
        text(&three).contains("Escalation: timed out for 1 hour"),
        "{three:?}"
    );
    assert_eq!(cases_of_kind(&harness, MEMBER, "timeout").await, 2);
}

#[tokio::test]
async fn a_revoked_warning_is_not_counted() {
    let harness = harness();
    harness.set_config(json!({
        "escalation": [{ "warns": 3, "action": "timeout", "duration": "1h" }]
    }));
    warn_times(&harness, MEMBER, 2).await;
    harness.invoke(case_revoke(1, None)).await.unwrap();

    // Two stand now: the second and this one.
    let third = warn_times(&harness, MEMBER, 1).await;
    assert!(!text(&third).contains("Escalation"), "{third:?}");
    let fourth = warn_times(&harness, MEMBER, 1).await;
    assert!(
        text(&fourth).contains("Escalation: timed out for 1 hour"),
        "{fourth:?}"
    );
    assert_eq!(cases_of_kind(&harness, MEMBER, "timeout").await, 1);
}

#[tokio::test]
async fn the_escalation_speaks_the_server_language_in_its_reason_and_reply() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness.set_config(json!({
        "escalation": [{ "warns": 1, "action": "timeout", "duration": "1h" }]
    }));
    let replies = harness
        .invoke(warn(MEMBER, Some("荒らし")).locale(Locale::Ja))
        .await
        .unwrap();
    assert!(
        text(&replies).contains("1時間のタイムアウト"),
        "{replies:?}"
    );
    let case = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(
        case.reason.as_deref(),
        Some("自動: 有効な警告が 1 件 (ケース #1)")
    );
}

#[test]
fn the_escalation_rules_are_checked_against_their_schema() {
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
    let one = |rule: serde_json::Value| accepted(json!({ "escalation": [rule] }));
    assert!(one(
        json!({ "warns": 3, "action": "timeout", "duration": "1h" })
    ));
    assert!(one(json!({ "warns": 1, "action": "kick" })));
    assert!(one(
        json!({ "warns": 100, "action": "ban", "duration": "7d" })
    ));
    assert!(one(json!({ "warns": 5, "action": "ban" })));
    assert!(!one(json!({ "warns": 0, "action": "kick" })));
    assert!(!one(json!({ "warns": 101, "action": "kick" })));
    assert!(!one(json!({ "warns": 3, "action": "mute" })));
    assert!(!one(
        json!({ "warns": 3, "action": "timeout", "duration": "banana" })
    ));
    assert!(!one(json!({ "warns": 3, "action": "kick", "expire": 1 })));

    let rule = json!({ "warns": 3, "action": "kick" });
    assert!(accepted(json!({ "escalation": vec![rule.clone(); 10] })));
    assert!(!accepted(json!({ "escalation": vec![rule; 11] })));
}

// ---- timeouts by role, and voice mutes ----

const TIMEOUT_ROLE: u64 = 30;
const VOICE_ROLE: u64 = 31;

fn voice_mute(target: u64, duration: Option<&str>, reason: Option<&str>) -> Invocation {
    let invocation = Invocation::new("vcmute").option("user", user(target));
    let invocation = match duration {
        Some(duration) => {
            invocation.option("duration", CommandOptionValue::String(duration.to_owned()))
        }
        None => invocation,
    };
    by_moderator(with_reason(invocation, reason))
}

fn voice_unmute(target: u64, reason: Option<&str>) -> Invocation {
    by_moderator(with_reason(
        Invocation::new("vcunmute").option("user", user(target)),
        reason,
    ))
}

/// Whether the module added (`added`) or removed `role` from anyone.
fn role_changed(harness: &Harness, role: u64, added: bool) -> bool {
    harness.api().actions().iter().any(|action| match action {
        Action::AddRole { role_id, .. } => added && *role_id == Id::new(role),
        Action::RemoveRole { role_id, .. } => !added && *role_id == Id::new(role),
        _ => false,
    })
}

/// Fills the scheduler of the module, so that the next job is refused.
async fn fill_scheduler(harness: &Harness) {
    for n in 0..10_000_u32 {
        harness
            .scheduler()
            .job("filler", &n)
            .unwrap()
            .after(Duration::from_secs(60))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_timeout_by_role_adds_the_role_and_a_timer_removes_it_when_due() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    let replies = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm"], "{replies:?}");
    assert!(role_changed(&harness, TIMEOUT_ROLE, true));
    assert!(
        !harness
            .api()
            .actions()
            .iter()
            .any(|action| matches!(action, Action::Timeout { .. })),
        "Discord's own timeout is not used"
    );

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "timeout");
    assert_eq!(case.expires_at, Some(case.created_at + 3_600));
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_timeout_by_role_does_nothing_without_a_usable_role() {
    let harness = harness();
    for config in [
        json!({ "timeout_method": "role" }),
        json!({ "timeout_method": "role", "timeout_role": "0" }),
    ] {
        harness.set_config(config);
        harness.advance(Duration::from_secs(3));
        let replies = harness
            .invoke(timeout(MEMBER, "1h", Some("spam")))
            .await
            .unwrap();
        assert!(text(&replies).contains("timeout_role"), "{replies:?}");
    }
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_timeout_by_role_may_last_longer_than_discord_allows() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    let replies = harness
        .invoke(timeout(MEMBER, "29d", Some("spam")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm"]);
    assert_eq!(harness.jobs(), 1);
}

#[tokio::test]
async fn a_timeout_by_role_holds_when_its_timer_cannot_be_set() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    fill_scheduler(&harness).await;
    let replies = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(text(&replies).contains("timer"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm"]);
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
}

#[tokio::test]
async fn an_untimeout_by_role_removes_the_role_and_cancels_the_timer() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(untimeout(MEMBER, Some("sorry")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);
    assert_eq!(harness.jobs(), 0, "the timer is cancelled");

    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "untimeout");
    let timed = harness.cases().get(1).await.unwrap().unwrap();
    let revoked = timed.revoked.expect("the timeout is revoked");
    assert_eq!(revoked.reason.as_deref(), Some("removed by case #2"));
}

#[tokio::test]
async fn an_untimeout_by_role_needs_the_role_too() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role" }));
    let replies = harness.invoke(untimeout(MEMBER, None)).await.unwrap();
    assert!(text(&replies).contains("timeout_role"), "{replies:?}");
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn a_timed_out_role_is_the_one_the_timeout_gave_even_after_a_config_change() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "32" }));

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert!(role_changed(&harness, TIMEOUT_ROLE, false));
    assert!(!role_changed(&harness, 32, false));
}

#[tokio::test]
async fn an_escalated_timeout_by_role_adds_the_role() {
    let harness = harness();
    harness.set_config(json!({
        "timeout_method": "role",
        "timeout_role": "30",
        "escalation": [{ "warns": 1, "action": "timeout", "duration": "1h" }]
    }));
    let replies = harness.invoke(warn(MEMBER, Some("spam"))).await.unwrap();
    assert!(
        text(&replies).contains("Escalation: timed out for 1 hour — case #2."),
        "{replies:?}"
    );
    assert_eq!(sequence(&harness), ["dm", "role+", "dm"]);
    let case = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "timeout");
    assert_eq!(harness.jobs(), 1);
}

#[tokio::test]
async fn a_voice_mute_in_discord_is_recorded_and_messaged_after_it() {
    let harness = harness();
    let replies = harness
        .invoke(voice_mute(MEMBER, None, Some("noisy")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm"], "{replies:?}");

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "vcmute");
    assert_eq!(case.expires_at, None);
    assert_eq!(case.reason.as_deref(), Some("noisy"));
    assert!(dm_contents(&harness)[0].contains("noisy"));
    assert_eq!(harness.jobs(), 0, "no duration, no timer");
}

#[tokio::test]
async fn a_voice_mute_with_a_duration_is_lifted_when_its_time_is_up() {
    let harness = harness();
    let replies = harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    assert!(text(&replies).contains("1 hour"), "{replies:?}");
    assert!(dm_contents(&harness)[0].contains("1 hour"));
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.expires_at, Some(case.created_at + 3_600));
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm", "vcunmute"]);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_mute_the_discord_refuses_for_a_member_out_of_voice_leaves_no_case() {
    let harness = harness();
    harness.set_log_channel(50);
    // The lookup of the member is the first call, the mute the second.
    harness.api().fail_nth(2, DiscordError::NotInVoice);
    let result = harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("noisy")))
        .await;
    assert!(
        matches!(result, Err(ModuleError::Discord(DiscordError::NotInVoice))),
        "{result:?}"
    );
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert!(sequence(&harness).is_empty());
    assert!(harness.sent_embeds().is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_mute_by_role_adds_the_voice_role_and_lifts_it_when_due() {
    let harness = harness();
    harness.set_config(json!({ "voice_mute_method": "role", "voice_mute_role": "31" }));
    harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    assert_eq!(sequence(&harness), ["role+", "dm"]);
    assert!(role_changed(&harness, VOICE_ROLE, true));
    assert_eq!(cases_of_kind(&harness, MEMBER, "vcmute").await, 1);
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);
    assert!(role_changed(&harness, VOICE_ROLE, false));
}

#[tokio::test]
async fn a_voice_mute_by_role_needs_its_role() {
    let harness = harness();
    harness.set_config(json!({ "voice_mute_method": "role" }));
    let replies = harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    assert!(text(&replies).contains("voice_mute_role"), "{replies:?}");
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_unmute_lifts_the_mute_cancels_its_timer_and_revokes_it() {
    let harness = harness();
    harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(voice_unmute(MEMBER, Some("enough")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm", "vcunmute"]);
    assert_eq!(harness.jobs(), 0, "the timer is cancelled");

    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "vcunmute");
    let muted = harness.cases().get(1).await.unwrap().unwrap();
    let revoked = muted.revoked.expect("the mute is revoked");
    assert_eq!(revoked.reason.as_deref(), Some("removed by case #2"));
}

#[tokio::test]
async fn a_voice_unmute_the_discord_refuses_leaves_the_mute_standing() {
    let harness = harness();
    harness
        .invoke(voice_mute(MEMBER, None, Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness.api().fail_nth(2, DiscordError::NotInVoice);
    let result = harness.invoke(voice_unmute(MEMBER, None)).await;
    assert!(
        matches!(result, Err(ModuleError::Discord(DiscordError::NotInVoice))),
        "{result:?}"
    );
    assert_eq!(cases_of_kind(&harness, MEMBER, "vcunmute").await, 0);
    let muted = harness.cases().get(1).await.unwrap().unwrap();
    assert!(muted.revoked.is_none());
}

#[tokio::test]
async fn a_voice_unmute_by_role_removes_the_role_and_needs_it() {
    let harness = harness();
    harness.set_config(json!({ "voice_mute_method": "role", "voice_mute_role": "31" }));
    harness
        .invoke(voice_mute(MEMBER, None, Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(voice_unmute(MEMBER, None)).await.unwrap();
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);

    harness.set_config(json!({ "voice_mute_method": "role" }));
    harness.advance(Duration::from_secs(3));
    let replies = harness.invoke(voice_unmute(MEMBER, None)).await.unwrap();
    assert!(text(&replies).contains("voice_mute_role"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);
}

#[tokio::test]
async fn a_voice_lift_the_discord_refuses_for_now_is_tried_ten_minutes_later() {
    let harness = harness();
    harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3_600));
    harness.api().fail_nth(1, DiscordError::NotInVoice);
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(harness.jobs(), 1, "postponed, not given up");
    assert_eq!(sequence(&harness), ["vcmute", "dm"], "nothing lifted yet");

    harness.advance(Duration::from_secs(599));
    assert!(harness.run_due_jobs().await.is_empty(), "not yet");
    harness.advance(Duration::from_secs(1));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm", "vcunmute"]);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_lift_gives_up_a_day_after_it_fell_due() {
    let harness = harness();
    harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3_600));
    harness.api().fail_with(DiscordError::NotInVoice);
    let mut runs = 0;
    while harness.jobs() > 0 {
        assert!(runs < 200, "the lift never gives up");
        let ended = harness.run_due_jobs().await;
        assert!(ended.iter().all(Result::is_ok), "{ended:?}");
        runs += 1;
        harness.advance(Duration::from_secs(600));
    }
    // About a day of tries, ten minutes apart, then it is dropped. The mock records a refused
    // call too, so each try is one `vcunmute` in the sequence.
    assert!((144..=147).contains(&runs), "{runs} runs");
    let attempts = sequence(&harness);
    assert_eq!(attempts[..2], ["vcmute", "dm"]);
    assert!(
        attempts[2..].iter().all(|step| *step == "vcunmute"),
        "{attempts:?}"
    );
    assert_eq!(attempts.len(), 2 + runs);
}

#[tokio::test]
async fn a_voice_lift_by_role_is_done_when_the_member_or_role_is_gone() {
    let harness = harness();
    harness.set_config(json!({ "voice_mute_method": "role", "voice_mute_role": "31" }));
    harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3_600));
    harness.api().fail_nth(1, DiscordError::NotFound);
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(harness.jobs(), 0);
    assert_eq!(sequence(&harness), ["role+", "dm"]);
}

#[tokio::test]
async fn the_voice_commands_need_the_voice_permission() {
    let harness = harness();
    let denied = harness
        .invoke(
            Invocation::new("vcmute")
                .option("user", user(MEMBER))
                .user(UNPRIVILEGED),
        )
        .await
        .unwrap();
    assert!(text(&denied).contains("level"), "{denied:?}");
    let denied = harness
        .invoke(
            Invocation::new("vcunmute")
                .option("user", user(MEMBER))
                .user(UNPRIVILEGED),
        )
        .await
        .unwrap();
    assert!(text(&denied).contains("level"), "{denied:?}");
    assert!(sequence(&harness).is_empty());
}

#[test]
fn the_restriction_settings_are_checked_against_their_schema() {
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
    assert!(accepted(
        json!({ "timeout_method": "role", "timeout_role": "123" })
    ));
    assert!(accepted(json!({ "timeout_method": "discord" })));
    assert!(accepted(
        json!({ "voice_mute_method": "role", "voice_mute_role": "456" })
    ));
    assert!(accepted(json!({ "voice_mute_role": null })));
    assert!(!accepted(json!({ "timeout_method": "mute" })));
    assert!(!accepted(json!({ "voice_mute_method": "timeout" })));
    assert!(!accepted(json!({ "timeout_role": "role-name" })));
    assert!(!accepted(
        json!({ "voice_mute_role": "123456789012345678901" })
    ));
    assert!(!accepted(json!({ "timeout_role": 123 })));
}

// ---- templates ----

/// The settings with the templates of one punishment.
fn templates_for(kind: &str, templates: serde_json::Value) -> serde_json::Value {
    let mut section = serde_json::Map::new();
    section.insert(kind.to_owned(), templates);
    json!({ "templates": section })
}

/// The select menu of a reply, if it has one.
fn menu_of(reply: &Reply) -> Option<SelectMenu> {
    reply
        .components
        .iter()
        .find_map(|component| match component {
            Component::ActionRow(row) => row.components.iter().find_map(|inner| match inner {
                Component::SelectMenu(menu) => Some(menu.clone()),
                _ => None,
            }),
            _ => None,
        })
}

/// The options of the picker as `(label, value, description)`.
fn options_of(reply: &Reply) -> Vec<(String, String, Option<String>)> {
    menu_of(reply)
        .expect("a select menu")
        .options
        .unwrap_or_default()
        .into_iter()
        .map(|option| (option.label, option.value, option.description))
        .collect()
}

/// The reply that replaced the picker, if the pick changed it.
fn edit_of(replies: &[Reply]) -> Option<&Reply> {
    replies.iter().find(|reply| reply.kind == ReplyKind::Edit)
}

/// The custom id of the picker that the command answered with.
fn picker_id(replies: &[Reply]) -> String {
    replies[0].custom_ids()[0].clone()
}

/// Picks `template` in the picker `custom_id` as `user`, who has `roles`.
async fn pick_as(
    harness: &Harness,
    custom_id: &str,
    template: &'static str,
    user: u64,
    roles: &[u64],
) -> Vec<Reply> {
    harness
        .click(
            Click::new(custom_id)
                .values([template])
                .user(user)
                .roles(roles.iter().copied()),
        )
        .await
        .unwrap()
}

/// Picks `template` as the moderator, who has the moderator role.
async fn pick(harness: &Harness, custom_id: &str, template: &'static str) -> Vec<Reply> {
    pick_as(harness, custom_id, template, MODERATOR, &[MODERATOR_ROLE]).await
}

/// Whether the settings are accepted by the module's schema.
fn accepted_settings(config: serde_json::Value) -> bool {
    let registry = Arc::new(
        ModuleRegistry::new(vec![Arc::new(module().unwrap()) as Arc<dyn Module>]).unwrap(),
    );
    let compiler = ConfigCompiler::new(registry).unwrap();
    let mut moderation = serde_json::Map::new();
    moderation.insert("config".to_owned(), config);
    // JSON is YAML, so the settings go in as one flow mapping.
    let yaml = format!(
        "modules:\n  moderation: {}\n",
        serde_json::Value::Object(moderation)
    );
    compiler.compile(&parse_overrides(&yaml).unwrap()).is_ok()
}

#[tokio::test]
async fn a_punishment_without_a_reason_asks_for_a_template_and_does_nothing_yet() {
    let harness = harness();
    harness.set_config(templates_for(
        "kick",
        json!([{ "id": "spam", "label": "Spam", "reason": "Spam in the server" }]),
    ));
    let replies = harness.invoke(kick(MEMBER, None)).await.unwrap();

    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0].kind, ReplyKind::Response);
    assert!(replies[0].ephemeral, "the picker is private by default");
    assert_eq!(replies[0].embeds.len(), 1, "{replies:?}");
    assert!(replies[0].content.contains("<@7>"), "{replies:?}");
    let menu = menu_of(&replies[0]).expect("a select menu");
    assert_eq!(menu.placeholder.as_deref(), Some("Template"));
    let ids = replies[0].custom_ids();
    assert_eq!(ids.len(), 1, "{ids:?}");
    let owned_by_moderator = format!("moderation:tpl:{MODERATOR}:");
    assert!(ids[0].starts_with(&owned_by_moderator), "{ids:?}");
    assert_eq!(
        options_of(&replies[0]),
        [(
            "Spam".to_owned(),
            "spam".to_owned(),
            Some("Spam in the server".to_owned())
        )]
    );

    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_picked_template_runs_the_punishment_and_replaces_the_picker() {
    let harness = harness();
    harness.set_log_channel(50);
    harness.set_config(templates_for(
        "kick",
        json!([{ "id": "spam", "label": "Spam", "reason": "Spam in the general channel" }]),
    ));
    let replies = harness.invoke(kick(MEMBER, None)).await.unwrap();
    let picked = pick(&harness, &picker_id(&replies), "spam").await;

    let edit = edit_of(&picked).expect("the picker is replaced");
    assert!(edit.content.contains("case #1"), "{picked:?}");
    assert!(edit.components.is_empty(), "{picked:?}");
    assert_eq!(sequence(&harness), ["dm", "kick"]);

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "kick");
    assert_eq!(case.target, Id::new(MEMBER));
    assert_eq!(case.moderator, Id::new(MODERATOR));
    assert_eq!(case.reason.as_deref(), Some("Spam in the general channel"));
    assert!(dm_contents(&harness)[0].contains("Spam in the general channel"));
    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), 1, "{embeds:?}");
    assert_eq!(embeds[0].title.as_deref(), Some("Case #1 · Kick"));
}

#[tokio::test]
async fn a_timeout_without_a_duration_takes_the_template_duration_and_keeps_the_given_reason() {
    let harness = harness();
    harness.set_config(templates_for(
        "timeout",
        json!([{ "id": "short", "label": "Short", "reason": "Template reason", "duration": "2h" }]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, Some("spam")))
        .await
        .unwrap();
    assert!(menu_of(&replies[0]).is_some(), "{replies:?}");

    let picked = pick(&harness, &picker_id(&replies), "short").await;
    assert!(
        edit_of(&picked)
            .expect("the picker is replaced")
            .content
            .contains("2 hours"),
        "{picked:?}"
    );
    assert_eq!(sequence(&harness), ["timeout", "dm"]);
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        case.reason.as_deref(),
        Some("spam"),
        "the given reason is kept"
    );
    assert_eq!(case.expires_at, Some(case.created_at + 7_200));
}

#[tokio::test]
async fn a_ban_without_a_reason_takes_the_template_reason_and_a_given_duration_wins() {
    let harness = harness();
    harness.set_config(templates_for(
        "ban",
        json!([{ "id": "raid", "label": "Raid", "reason": "Raid", "duration": "7d" }]),
    ));
    let replies = harness
        .invoke(ban(MEMBER, None, Some("2h"), None))
        .await
        .unwrap();
    let picked = pick_as(
        &harness,
        &picker_id(&replies),
        "raid",
        MODERATOR,
        &[ADMIN_ROLE],
    )
    .await;
    assert!(edit_of(&picked).is_some(), "{picked:?}");

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.reason.as_deref(), Some("Raid"));
    assert_eq!(case.expires_at, Some(case.created_at + 7_200));
    assert_eq!(harness.jobs(), 1, "the timer of the two hour ban");
    assert_eq!(sequence(&harness), ["dm", "ban"]);
    assert!(dm_contents(&harness)[0].contains("2 hours"));
}

#[tokio::test]
async fn a_ban_without_a_duration_is_for_good_and_a_ban_with_a_reason_asks_nothing() {
    let harness = harness();
    harness.set_config(templates_for(
        "ban",
        json!([{ "id": "raid", "label": "Raid", "reason": "Raid" }]),
    ));
    let given = harness
        .invoke(ban(MEMBER, Some("spam"), None, None))
        .await
        .unwrap();
    assert!(menu_of(&given[0]).is_none(), "{given:?}");
    assert_eq!(sequence(&harness), ["dm", "ban"]);
    assert_eq!(harness.jobs(), 0);

    harness.advance(Duration::from_secs(3));
    let replies = harness.invoke(ban(FREE, None, None, None)).await.unwrap();
    let picked = pick_as(
        &harness,
        &picker_id(&replies),
        "raid",
        MODERATOR,
        &[ADMIN_ROLE],
    )
    .await;
    assert!(edit_of(&picked).is_some(), "{picked:?}");
    let case = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(case.reason.as_deref(), Some("Raid"));
    assert_eq!(case.expires_at, None, "no duration, no timer");
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_mute_template_with_a_duration_is_lifted_when_it_is_due() {
    let harness = harness();
    harness.set_config(templates_for(
        "vcmute",
        json!([{ "id": "noisy", "label": "Noisy", "reason": "Noisy", "duration": "1h" }]),
    ));
    let replies = harness
        .invoke(voice_mute(MEMBER, None, None))
        .await
        .unwrap();
    let picked = pick(&harness, &picker_id(&replies), "noisy").await;
    assert!(edit_of(&picked).is_some(), "{picked:?}");

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "vcmute");
    assert_eq!(case.reason.as_deref(), Some("Noisy"));
    assert_eq!(case.expires_at, Some(case.created_at + 3_600));
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm", "vcunmute"]);
}

#[tokio::test]
async fn a_timeout_with_a_reason_and_a_duration_asks_nothing() {
    let harness = harness();
    harness.set_config(templates_for(
        "timeout",
        json!([{ "id": "short", "label": "Short", "reason": "Template", "duration": "2h" }]),
    ));
    let replies = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert!(menu_of(&replies[0]).is_none(), "{replies:?}");
    assert_eq!(sequence(&harness), ["timeout", "dm"]);
    let cases = cases_of(&harness, MEMBER).await;
    assert_eq!(cases[0].reason.as_deref(), Some("spam"));
    assert_eq!(cases[0].expires_at, Some(cases[0].created_at + 3_600));
}

#[tokio::test]
async fn without_a_usable_template_a_punishment_runs_without_a_reason() {
    let configs = [
        json!({}),
        json!({ "templates": { "timeout": [], "kick": [], "ban": [], "vcmute": [] } }),
        json!({ "templates": {
            "timeout": [{ "id": "a", "label": "A", "reason": "r" }],
            "kick": [{ "id": "a", "label": "A", "reason": "r", "duration": "banana" }],
            "ban": [{ "id": "a", "label": "A", "reason": "r", "duration": "banana" }],
            "vcmute": [{ "id": "a", "label": "A", "reason": "r", "duration": "banana" }]
        } }),
    ];
    let harness = harness();
    for config in configs {
        harness.set_config(config);
        for invocation in [
            timeout_of(MEMBER, Some("1h"), None),
            kick(MEMBER, None),
            ban(MEMBER, None, None, None),
            voice_mute(MEMBER, None, None),
        ] {
            harness.advance(Duration::from_secs(3));
            let replies = harness.invoke(invocation).await.unwrap();
            assert!(menu_of(&replies[0]).is_none(), "{replies:?}");
        }
    }
    let cases = cases_of(&harness, MEMBER).await;
    assert_eq!(cases.len(), 12, "{cases:?}");
    assert!(cases.iter().all(|case| case.reason.is_none()), "{cases:?}");
}

#[tokio::test]
async fn a_timeout_without_a_duration_or_a_usable_template_needs_one() {
    let harness = harness();
    let replies = harness
        .invoke(timeout_of(MEMBER, None, Some("spam")))
        .await
        .unwrap();
    assert!(text(&replies).contains("Give a duration"), "{replies:?}");
    assert!(menu_of(&replies[0]).is_none(), "{replies:?}");
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn with_require_reason_a_punishment_without_a_reason_or_template_is_refused() {
    let harness = harness();
    harness.set_config(json!({ "require_reason": true }));
    let replies = harness.invoke(kick(MEMBER, None)).await.unwrap();
    assert!(
        text(&replies).contains("A reason is required"),
        "{replies:?}"
    );
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn a_template_that_cannot_be_used_is_not_offered() {
    let harness = harness();
    harness.set_config(templates_for(
        "timeout",
        json!([
            { "id": "bad", "label": "Bad", "reason": "r", "duration": "banana" },
            { "id": "open", "label": "Open", "reason": "r" },
            { "id": "long", "label": "Long", "reason": "r", "duration": "29d" },
            { "id": "ok", "label": "Ok", "reason": "r", "duration": "1h" },
            { "id": "ok", "label": "Again", "reason": "r", "duration": "2h" }
        ]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, Some("spam")))
        .await
        .unwrap();
    assert_eq!(
        options_of(&replies[0]),
        [(
            "Ok".to_owned(),
            "ok".to_owned(),
            Some("r (1 hour)".to_owned())
        )]
    );

    // Discord's limit of 28 days is for its own timeout only.
    harness.set_config(json!({
        "timeout_method": "role",
        "timeout_role": "30",
        "templates": { "timeout": [{ "id": "long", "label": "Long", "reason": "r", "duration": "29d" }] }
    }));
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, Some("spam")))
        .await
        .unwrap();
    assert_eq!(options_of(&replies[0]).len(), 1, "{replies:?}");
}

#[tokio::test]
async fn an_option_description_keeps_the_duration_within_discord_s_limit() {
    let harness = harness();
    let long_reason = "a".repeat(300);
    harness.set_config(templates_for(
        "ban",
        json!([{ "id": "long", "label": "Long", "reason": long_reason, "duration": "1h" }]),
    ));
    let replies = harness.invoke(ban(MEMBER, None, None, None)).await.unwrap();
    let description = options_of(&replies[0])[0].2.clone().expect("a description");
    assert!(description.chars().count() <= 100, "{description}");
    assert!(description.ends_with(" (1 hour)"), "{description}");
}

#[tokio::test]
async fn a_picker_is_only_for_the_moderator_who_ran_the_command() {
    let harness = harness();
    harness.set_config(templates_for(
        "kick",
        json!([{ "id": "spam", "label": "Spam", "reason": "Spam" }]),
    ));
    let replies = harness.invoke(kick(MEMBER, None)).await.unwrap();
    let id = picker_id(&replies);

    let stranger = pick_as(&harness, &id, "spam", UNPRIVILEGED, &[]).await;
    assert!(stranger[0].content.contains("not for you"), "{stranger:?}");
    assert!(sequence(&harness).is_empty());

    let owner = pick(&harness, &id, "spam").await;
    assert!(edit_of(&owner).is_some(), "{owner:?}");
    assert_eq!(sequence(&harness), ["dm", "kick"]);
}

#[tokio::test]
async fn a_second_pick_and_a_pick_after_ten_minutes_say_the_selection_expired() {
    let harness = harness();
    harness.set_config(templates_for(
        "kick",
        json!([{ "id": "spam", "label": "Spam", "reason": "Spam" }]),
    ));
    let replies = harness.invoke(kick(MEMBER, None)).await.unwrap();
    let id = picker_id(&replies);
    assert!(edit_of(&pick(&harness, &id, "spam").await).is_some());
    let again = pick(&harness, &id, "spam").await;
    assert!(again[0].content.contains("Expired"), "{again:?}");
    assert_eq!(sequence(&harness), ["dm", "kick"], "the kick happened once");

    let late = crate::harness();
    late.set_config(templates_for(
        "kick",
        json!([{ "id": "spam", "label": "Spam", "reason": "Spam" }]),
    ));
    let replies = late.invoke(kick(MEMBER, None)).await.unwrap();
    late.advance(Duration::from_secs(601));
    let expired = pick(&late, &picker_id(&replies), "spam").await;
    assert!(expired[0].content.contains("Expired"), "{expired:?}");
    assert!(sequence(&late).is_empty());
    assert!(cases_of(&late, MEMBER).await.is_empty());
}

#[tokio::test]
async fn a_template_removed_before_the_pick_is_gone_and_nothing_is_done() {
    let harness = harness();
    harness.set_config(templates_for(
        "kick",
        json!([{ "id": "spam", "label": "Spam", "reason": "Spam" }]),
    ));
    let replies = harness.invoke(kick(MEMBER, None)).await.unwrap();
    harness.set_config(json!({}));
    let gone = pick(&harness, &picker_id(&replies), "spam").await;
    assert!(gone[0].content.contains("was removed"), "{gone:?}");
    assert!(sequence(&harness).is_empty());
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn a_pick_by_someone_who_lost_the_permission_is_refused_and_the_admin_can_still_pick() {
    let harness = harness();
    harness.set_config(templates_for(
        "ban",
        json!([{ "id": "raid", "label": "Raid", "reason": "Raid" }]),
    ));
    let replies = harness.invoke(ban(MEMBER, None, None, None)).await.unwrap();
    let id = picker_id(&replies);

    let denied = pick_as(&harness, &id, "raid", MODERATOR, &[MODERATOR_ROLE]).await;
    assert!(denied[0].content.contains("level"), "{denied:?}");
    assert!(sequence(&harness).is_empty());

    let done = pick_as(&harness, &id, "raid", MODERATOR, &[ADMIN_ROLE]).await;
    assert!(edit_of(&done).is_some(), "{done:?}");
    assert_eq!(sequence(&harness), ["dm", "ban"]);
}

#[test]
fn the_templates_are_checked_against_their_schema() {
    let ban_templates = |entries: serde_json::Value| json!({ "templates": { "ban": entries } });
    assert!(accepted_settings(json!({ "templates": {
        "timeout": [{ "id": "short-1_x", "label": "Short", "reason": "Spam", "duration": "1h 30m" }],
        "kick": [{ "id": "a", "label": "A", "reason": "r" }],
        "ban": [],
        "vcmute": []
    } })));
    assert!(!accepted_settings(ban_templates(json!([
        { "id": "a b", "label": "A", "reason": "r" }
    ]))));
    assert!(!accepted_settings(ban_templates(json!([
        { "id": "a", "label": "", "reason": "r" }
    ]))));
    assert!(!accepted_settings(ban_templates(json!([
        { "id": "a", "label": "A" }
    ]))));
    assert!(!accepted_settings(ban_templates(json!([
        { "id": "a", "label": "A", "reason": "r", "colour": "red" }
    ]))));
    assert!(!accepted_settings(ban_templates(json!([
        { "id": "a", "label": "A", "reason": "r", "duration": "banana" }
    ]))));
    assert!(!accepted_settings(json!({ "templates": { "warn": [] } })));
    let template = json!({ "id": "a", "label": "A", "reason": "r" });
    assert!(accepted_settings(ban_templates(json!(vec![
        template
            .clone();
        25
    ]))));
    assert!(!accepted_settings(ban_templates(json!(vec![template; 26]))));
}

// ---- undo buttons ----

/// The buttons of a reply, as `(label, custom_id)`.
fn buttons_of(reply: &Reply) -> Vec<(String, String)> {
    reply
        .components
        .iter()
        .flat_map(|component| match component {
            Component::ActionRow(row) => row
                .components
                .iter()
                .filter_map(|inner| match inner {
                    Component::Button(button) => Some((
                        button.label.clone().unwrap_or_default(),
                        button.custom_id.clone().unwrap_or_default(),
                    )),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

/// The undo button of a reply, as `(label, custom_id)`.
fn undo_of(reply: &Reply) -> Option<(String, String)> {
    buttons_of(reply)
        .into_iter()
        .find(|(_, custom_id)| custom_id.starts_with("moderation:undo:"))
}

/// The custom id of the undo button of the first reply.
fn undo_id(replies: &[Reply]) -> String {
    undo_of(&replies[0]).expect("an undo button").1
}

fn undo_id_of_case(kind: &str, target: u64, number: i64) -> String {
    format!("moderation:undo:*:{kind}:{target}:{number}")
}

/// Presses the button `custom_id` as `user`, who has `roles`.
async fn press_as(harness: &Harness, custom_id: &str, user: u64, roles: &[u64]) -> Vec<Reply> {
    harness
        .click(
            Click::new(custom_id)
                .user(user)
                .roles(roles.iter().copied()),
        )
        .await
        .unwrap()
}

/// Presses the button as the moderator.
async fn press(harness: &Harness, custom_id: &str) -> Vec<Reply> {
    press_as(harness, custom_id, MODERATOR, &[MODERATOR_ROLE]).await
}

/// How often the module asked Discord for the guild.
fn guild_fetches(harness: &Harness) -> usize {
    harness
        .api()
        .actions()
        .iter()
        .filter(|action| matches!(action, Action::GetGuild { .. }))
        .count()
}

#[tokio::test]
async fn a_timeout_ban_vcmute_and_warn_reply_carry_an_undo_button_and_a_kick_or_note_does_not() {
    let harness = harness();
    let timed = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert_eq!(
        undo_of(&timed[0]),
        Some(("Undo".to_owned(), undo_id_of_case("timeout", MEMBER, 1))),
        "{timed:?}"
    );

    harness.advance(Duration::from_secs(3));
    let banned = harness
        .invoke(ban(FREE, Some("raid"), None, None))
        .await
        .unwrap();
    assert_eq!(
        undo_of(&banned[0]),
        Some(("Undo".to_owned(), undo_id_of_case("ban", FREE, 2))),
        "{banned:?}"
    );

    harness.advance(Duration::from_secs(3));
    let muted = harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("loud")))
        .await
        .unwrap();
    assert_eq!(
        undo_of(&muted[0]),
        Some(("Undo".to_owned(), undo_id_of_case("vcmute", MEMBER, 3))),
        "{muted:?}"
    );

    harness.advance(Duration::from_secs(3));
    let warned = harness.invoke(warn(MEMBER, Some("rude"))).await.unwrap();
    assert_eq!(
        undo_of(&warned[0]),
        Some(("Revoke".to_owned(), undo_id_of_case("warn", MEMBER, 4))),
        "{warned:?}"
    );

    harness.advance(Duration::from_secs(3));
    let kicked = harness.invoke(kick(MEMBER, Some("rude"))).await.unwrap();
    assert!(buttons_of(&kicked[0]).is_empty(), "{kicked:?}");
    let noted = harness.invoke(note(MEMBER, "watch")).await.unwrap();
    assert!(buttons_of(&noted[0]).is_empty(), "{noted:?}");
}

#[tokio::test]
async fn pressing_undo_on_a_timeout_lifts_it_records_the_untimeout_and_replaces_the_message() {
    let harness = harness();
    let timed = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    let replies = press(&harness, &undo_id(&timed)).await;

    let edit = edit_of(&replies).expect("the message is replaced");
    assert!(
        edit.content
            .starts_with("-# ↩ Undone by <@100>.\nRemoved the timeout of <@7> — case #2."),
        "{replies:?}"
    );
    assert!(edit.components.is_empty(), "{replies:?}");
    assert!(!edit.ephemeral, "the replaced message is shared");
    assert_eq!(sequence(&harness), ["timeout", "dm", "untimeout"]);

    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "untimeout");
    assert_eq!(lifted.reason.as_deref(), Some("Undone from case #1."));
    assert_eq!(lifted.moderator, Id::new(MODERATOR));
    let undone = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        undone.revoked.map(|revoked| revoked.reason),
        Some(Some("removed by case #2".to_owned()))
    );
}

#[tokio::test]
async fn a_picked_timeout_replaces_the_picker_with_a_result_that_has_an_undo_button() {
    let harness = harness();
    harness.set_config(templates_for(
        "timeout",
        json!([{ "id": "short", "label": "Short", "reason": "Spam", "duration": "1h" }]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, Some("spam")))
        .await
        .unwrap();
    let picked = pick(&harness, &picker_id(&replies), "short").await;
    let edit = edit_of(&picked).expect("the picker is replaced");
    let undo = undo_of(edit).map(|(_, custom_id)| custom_id);
    assert_eq!(
        undo,
        Some(undo_id_of_case("timeout", MEMBER, 1)),
        "{picked:?}"
    );

    let undone = press(&harness, &undo.unwrap_or_default()).await;
    assert!(
        edit_of(&undone)
            .expect("the message is replaced")
            .content
            .contains("Removed the timeout of <@7>"),
        "{undone:?}"
    );
}

#[tokio::test]
async fn a_second_press_says_that_is_already_over_and_does_nothing_again() {
    let harness = harness();
    let timed = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    let id = undo_id(&timed);
    press(&harness, &id).await;
    let before = sequence(&harness);

    let again = press(&harness, &id).await;
    assert!(text(&again).contains("That is already over."), "{again:?}");
    assert!(edit_of(&again).is_none(), "{again:?}");
    assert_eq!(sequence(&harness), before, "no second action on Discord");
    assert!(harness.cases().get(3).await.unwrap().is_none());
}

#[tokio::test]
async fn a_press_after_the_un_command_ran_is_not_needed() {
    let harness = harness();
    let timed = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    let id = undo_id(&timed);
    harness.advance(Duration::from_secs(3));
    harness.invoke(untimeout(MEMBER, None)).await.unwrap();
    let before = sequence(&harness);

    let late = press(&harness, &id).await;
    assert!(text(&late).contains("That is already over."), "{late:?}");
    assert_eq!(sequence(&harness), before);
}

#[tokio::test]
async fn a_press_after_the_timeout_ran_out_is_not_needed() {
    let harness = harness();
    let timed = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    let id = undo_id(&timed);
    harness.advance(Duration::from_secs(2 * 3_600));
    let before = sequence(&harness);

    let late = press(&harness, &id).await;
    assert!(text(&late).contains("That is already over."), "{late:?}");
    assert_eq!(sequence(&harness), before);
}

#[tokio::test]
async fn a_ban_is_undone_only_by_an_administrator_and_a_timed_one_loses_its_timer() {
    let harness = harness();
    let banned = harness
        .invoke(ban(FREE, Some("raid"), Some("2h"), None))
        .await
        .unwrap();
    let id = undo_id(&banned);
    assert_eq!(harness.jobs(), 1, "the timer of the two hour ban");

    let refused = press(&harness, &id).await;
    assert!(edit_of(&refused).is_none(), "{refused:?}");
    assert!(
        !sequence(&harness).contains(&"unban"),
        "{:?}",
        sequence(&harness)
    );
    assert_eq!(harness.jobs(), 1);

    let lifted = press_as(&harness, &id, MODERATOR, &[ADMIN_ROLE]).await;
    assert!(
        edit_of(&lifted)
            .expect("the message is replaced")
            .content
            .contains("Unbanned <@42> — case #2."),
        "{lifted:?}"
    );
    assert_eq!(sequence(&harness).last(), Some(&"unban"));
    assert_eq!(harness.jobs(), 0, "the timer is cancelled");
    let lifted_case = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted_case.kind.as_str(), "unban");
    assert_eq!(lifted_case.reason.as_deref(), Some("Undone from case #1."));
}

#[tokio::test]
async fn pressing_undo_on_a_voice_mute_lifts_it_and_records_the_vcunmute() {
    let harness = harness();
    let muted = harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("loud")))
        .await
        .unwrap();
    let replies = press(&harness, &undo_id(&muted)).await;

    assert!(
        edit_of(&replies)
            .expect("the message is replaced")
            .content
            .contains("Removed the voice mute of <@7> — case #2."),
        "{replies:?}"
    );
    assert_eq!(sequence(&harness).last(), Some(&"vcunmute"));
    assert_eq!(harness.jobs(), 0, "the timer of the mute is cancelled");
    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "vcunmute");
    assert_eq!(lifted.reason.as_deref(), Some("Undone from case #1."));
}

#[tokio::test]
async fn pressing_undo_on_a_warning_revokes_it_without_touching_discord() {
    let harness = harness();
    let warned = harness.invoke(warn(MEMBER, Some("spam"))).await.unwrap();
    let id = undo_id(&warned);
    let before = sequence(&harness);

    let refused = press_as(&harness, &id, UNPRIVILEGED, &[]).await;
    assert!(edit_of(&refused).is_none(), "{refused:?}");
    assert!(cases_of(&harness, MEMBER).await[0].revoked.is_none());

    let replies = press(&harness, &id).await;
    let edit = edit_of(&replies).expect("the message is replaced");
    assert!(edit.content.contains("Case #1 was revoked."), "{replies:?}");
    assert!(edit.components.is_empty(), "{replies:?}");
    assert_eq!(sequence(&harness), before, "no action on Discord");

    let revoked = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        revoked.revoked.map(|revoked| revoked.reason),
        Some(Some("Undone with its button.".to_owned()))
    );
    let listed = harness.invoke(warnings(MEMBER)).await.unwrap();
    assert!(
        text(&listed).contains("has no active warnings"),
        "{listed:?}"
    );
}

#[tokio::test]
async fn a_timeout_press_without_the_permission_is_refused_and_nothing_happens() {
    let harness = harness();
    let timed = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    let before = sequence(&harness);

    let refused = press_as(&harness, &undo_id(&timed), UNPRIVILEGED, &[]).await;
    assert!(edit_of(&refused).is_none(), "{refused:?}");
    assert_eq!(sequence(&harness), before);
    assert!(cases_of(&harness, MEMBER).await[0].revoked.is_none());
}

#[tokio::test]
async fn a_press_whose_case_is_of_another_member_or_another_kind_is_refused() {
    let harness = harness();
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    let before = sequence(&harness);

    let other_member = press(&harness, &undo_id_of_case("timeout", FREE, 1)).await;
    assert!(
        text(&other_member).contains("That is already over."),
        "{other_member:?}"
    );
    // An administrator presses, so that the kind is what refuses it and not the permission.
    let other_kind = press_as(
        &harness,
        &undo_id_of_case("ban", MEMBER, 1),
        MODERATOR,
        &[ADMIN_ROLE],
    )
    .await;
    assert!(
        text(&other_kind).contains("That is already over."),
        "{other_kind:?}"
    );
    assert_eq!(sequence(&harness), before);
    assert!(cases_of(&harness, MEMBER).await[0].revoked.is_none());
}

// ---- history line ----

/// The last line of the reply's text, the history line when it has one.
fn last_line(replies: &[Reply]) -> &str {
    text(replies).lines().last().unwrap_or_default()
}

#[tokio::test]
async fn a_clean_member_gets_a_history_line_of_zeros() {
    let harness = harness();
    let replies = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert_eq!(
        last_line(&replies),
        "-# Before this: 0 active warnings, 0 cases.",
        "{replies:?}"
    );
}

#[tokio::test]
async fn the_history_counts_earlier_warnings_and_cases_but_not_the_new_one() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("a"))).await.unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(warn(MEMBER, Some("b"))).await.unwrap();
    for word in ["one", "two", "three"] {
        harness.invoke(note(MEMBER, word)).await.unwrap();
    }

    let replies = harness.invoke(kick(MEMBER, Some("rude"))).await.unwrap();
    assert_eq!(
        last_line(&replies),
        "-# Before this: 2 active warnings, 5 cases.",
        "{replies:?}"
    );
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 6);
}

#[tokio::test]
async fn one_warning_and_one_case_are_said_in_the_singular() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("a"))).await.unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert_eq!(
        last_line(&replies),
        "-# Before this: 1 active warning, 1 case.",
        "{replies:?}"
    );
}

#[tokio::test]
async fn the_history_line_is_in_japanese_for_a_japanese_moderator() {
    let harness = harness();
    harness.invoke(warn(MEMBER, Some("a"))).await.unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(timeout(MEMBER, "1h", Some("spam")).locale(Locale::Ja))
        .await
        .unwrap();
    assert_eq!(
        last_line(&replies),
        "-# 今回より前: 有効な警告 1 件、ケース 1 件。",
        "{replies:?}"
    );
}

#[tokio::test]
async fn a_history_of_a_hundred_cases_or_more_says_so_at_the_scan_limit() {
    let exactly = harness();
    for _ in 0..100 {
        exactly.invoke(note(MEMBER, "n")).await.unwrap();
    }
    let replies = exactly
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert_eq!(
        last_line(&replies),
        "-# Before this: 0 active warnings, 100 cases.",
        "{replies:?}"
    );

    let more = harness();
    for _ in 0..101 {
        more.invoke(note(MEMBER, "n")).await.unwrap();
    }
    let replies = more
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    assert_eq!(
        last_line(&replies),
        "-# Before this: 0 active warnings, 100+ cases.",
        "{replies:?}"
    );
}

#[tokio::test]
async fn the_history_line_is_on_the_picker_path_too_and_not_on_a_warning() {
    let harness = harness();
    harness.set_config(templates_for(
        "timeout",
        json!([{ "id": "short", "label": "Short", "reason": "Spam", "duration": "1h" }]),
    ));
    let warned = harness.invoke(warn(MEMBER, Some("a"))).await.unwrap();
    assert!(!text(&warned).contains("Before this"), "{warned:?}");

    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, Some("spam")))
        .await
        .unwrap();
    let picked = pick(&harness, &picker_id(&replies), "short").await;
    let edit = edit_of(&picked).expect("the picker is replaced");
    assert_eq!(
        edit.content.lines().last(),
        Some("-# Before this: 1 active warning, 1 case."),
        "{picked:?}"
    );
}

// ---- placeholders in template reasons ----

#[tokio::test]
async fn each_placeholder_is_expanded_in_the_stored_reason_and_the_direct_message() {
    let harness = harness();
    harness.api().set_guild_name(Id::new(GUILD), "Test Server");
    harness.set_config(templates_for(
        "timeout",
        json!([{
            "id": "raid",
            "label": "Raid",
            "reason": "Raid by {moderator} on {user} in {server}",
            "duration": "1h"
        }]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, None))
        .await
        .unwrap();
    let described = options_of(&replies[0]);
    assert!(
        described[0]
            .2
            .as_deref()
            .unwrap_or_default()
            .starts_with("Raid by {moderator} on {user} in {server}"),
        "the picker shows the template as written: {described:?}"
    );

    let picked = pick(&harness, &picker_id(&replies), "raid").await;
    assert!(edit_of(&picked).is_some(), "{picked:?}");
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        case.reason.as_deref(),
        Some("Raid by tester on <@7> in Test Server")
    );
    // The direct message escapes the text, which breaks the mention into plain text.
    assert!(
        dm_contents(&harness)[0].contains("Raid by tester on <\u{200B}@7\\> in Test Server"),
        "{:?}",
        dm_contents(&harness)
    );
}

#[tokio::test]
async fn the_server_is_asked_for_only_when_a_reason_names_it() {
    let harness = harness();
    harness.api().set_guild_name(Id::new(GUILD), "Test Server");
    harness.set_config(templates_for(
        "timeout",
        json!([
            { "id": "plain", "label": "Plain", "reason": "No server word", "duration": "1h" },
            { "id": "srv", "label": "Server", "reason": "Raid on {server}", "duration": "1h" }
        ]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, None))
        .await
        .unwrap();
    pick(&harness, &picker_id(&replies), "plain").await;
    assert_eq!(guild_fetches(&harness), 0);

    harness.advance(Duration::from_secs(3));
    let replies = harness.invoke(timeout_of(FREE, None, None)).await.unwrap();
    let picked = pick(&harness, &picker_id(&replies), "srv").await;
    assert!(edit_of(&picked).is_some(), "{picked:?}");
    assert_eq!(guild_fetches(&harness), 1);
    assert_eq!(
        harness
            .cases()
            .get(2)
            .await
            .unwrap()
            .unwrap()
            .reason
            .as_deref(),
        Some("Raid on Test Server")
    );
}

#[tokio::test]
async fn a_reason_the_command_gives_wins_and_the_server_is_not_asked_for() {
    let harness = harness();
    harness.set_config(templates_for(
        "timeout",
        json!([{ "id": "srv", "label": "Server", "reason": "Raid on {server}", "duration": "1h" }]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, Some("typed")))
        .await
        .unwrap();
    let picked = pick(&harness, &picker_id(&replies), "srv").await;
    assert!(edit_of(&picked).is_some(), "{picked:?}");
    assert_eq!(guild_fetches(&harness), 0);
    assert_eq!(
        harness
            .cases()
            .get(1)
            .await
            .unwrap()
            .unwrap()
            .reason
            .as_deref(),
        Some("typed")
    );
}

#[tokio::test]
async fn an_unknown_word_stays_as_typed_and_a_typed_reason_is_not_expanded() {
    let harness = harness();
    harness.set_config(templates_for(
        "timeout",
        json!([{ "id": "odd", "label": "Odd", "reason": "{foo} for {user}", "duration": "1h" }]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, None))
        .await
        .unwrap();
    pick(&harness, &picker_id(&replies), "odd").await;
    assert_eq!(
        harness
            .cases()
            .get(1)
            .await
            .unwrap()
            .unwrap()
            .reason
            .as_deref(),
        Some("{foo} for <@7>")
    );

    harness.advance(Duration::from_secs(3));
    harness
        .invoke(timeout(FREE, "1h", Some("typed {user} {moderator}")))
        .await
        .unwrap();
    assert_eq!(
        harness
            .cases()
            .get(2)
            .await
            .unwrap()
            .unwrap()
            .reason
            .as_deref(),
        Some("typed {user} {moderator}")
    );
}

#[tokio::test]
async fn a_value_that_holds_a_placeholder_is_not_expanded_again() {
    let harness = harness();
    harness.api().set_guild_name(Id::new(GUILD), "{moderator}");
    harness.set_config(templates_for(
        "timeout",
        json!([{ "id": "srv", "label": "Server", "reason": "By {server}", "duration": "1h" }]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, None))
        .await
        .unwrap();
    pick(&harness, &picker_id(&replies), "srv").await;
    assert_eq!(
        harness
            .cases()
            .get(1)
            .await
            .unwrap()
            .unwrap()
            .reason
            .as_deref(),
        Some("By {moderator}")
    );
}

#[tokio::test]
async fn an_expanded_reason_is_cut_to_the_length_a_case_keeps() {
    let harness = harness();
    harness
        .api()
        .set_guild_name(Id::new(GUILD), &"b".repeat(100));
    let template = format!("{}{{server}}", "a".repeat(450));
    harness.set_config(templates_for(
        "timeout",
        json!([{ "id": "long", "label": "Long", "reason": template, "duration": "1h" }]),
    ));
    let replies = harness
        .invoke(timeout_of(MEMBER, None, None))
        .await
        .unwrap();
    let picked = pick(&harness, &picker_id(&replies), "long").await;
    assert!(edit_of(&picked).is_some(), "{picked:?}");
    let reason = harness
        .cases()
        .get(1)
        .await
        .unwrap()
        .unwrap()
        .reason
        .unwrap_or_default();
    assert_eq!(reason.chars().count(), 512);
    assert!(reason.ends_with('…'));
}

// ---- members who rejoin while a punishment stands ----

fn joined(user: u64, bot: bool) -> Event {
    Event::MemberJoined(MemberInfo {
        guild_id: Id::new(GUILD),
        user: UserInfo {
            id: Id::new(user),
            name: "member".to_owned(),
            bot,
        },
        roles: Vec::new(),
        nick: None,
        joined_at: None,
        timed_out_until: None,
    })
}

/// The member is back in the server, as the gateway announces it.
async fn rejoin(harness: &Harness, user: u64) -> Result<(), ModuleError> {
    harness.emit(joined(user, false)).await.map(drop)
}

/// The roles the module gave, in order.
fn roles_added(harness: &Harness) -> Vec<u64> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::AddRole { role_id, .. } => Some(role_id.get()),
            _ => None,
        })
        .collect()
}

/// The timeouts the module gave, in order: their seconds, or `None` for a lift.
fn timeouts_sent(harness: &Harness) -> Vec<Option<u32>> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::Timeout {
                duration_seconds, ..
            } => Some(*duration_seconds),
            _ => None,
        })
        .collect()
}

fn voice_mutes_sent(harness: &Harness) -> usize {
    harness
        .api()
        .actions()
        .iter()
        .filter(|action| matches!(action, Action::SetVoiceMute { mute: true, .. }))
        .count()
}

#[tokio::test]
async fn a_member_who_rejoins_gets_the_timeout_role_back_while_the_timeout_stands() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(roles_added(&harness), [TIMEOUT_ROLE, TIMEOUT_ROLE]);
    assert!(
        format!("{:?}", harness.api().actions().last().unwrap())
            .contains("Rejoined while the timeout of case #1 stood")
    );
}

#[tokio::test]
async fn an_untimeout_before_the_rejoin_means_no_timeout_role_again() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness.invoke(untimeout(MEMBER, None)).await.unwrap();
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(roles_added(&harness), [TIMEOUT_ROLE]);
}

#[tokio::test]
async fn a_timeout_that_ran_out_before_the_rejoin_is_not_given_again() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3_600));
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(roles_added(&harness), [TIMEOUT_ROLE]);
}

#[tokio::test]
async fn a_member_who_rejoins_gets_the_voice_role_back_while_the_mute_stands() {
    let harness = harness();
    harness.set_config(json!({ "voice_mute_method": "role", "voice_mute_role": "31" }));
    harness
        .invoke(voice_mute(MEMBER, None, Some("noise")))
        .await
        .unwrap();
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(roles_added(&harness), [VOICE_ROLE, VOICE_ROLE]);
}

#[tokio::test]
async fn a_voice_mute_by_discord_is_not_repeated_on_rejoin() {
    let harness = harness();
    harness
        .invoke(voice_mute(MEMBER, None, Some("noise")))
        .await
        .unwrap();
    let before = harness.api().actions().len();
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(harness.api().actions().len(), before);
    assert_eq!(voice_mutes_sent(&harness), 1);
}

#[tokio::test]
async fn a_timeout_by_discord_is_given_again_for_the_time_left() {
    let harness = harness();
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(1_200));
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(timeouts_sent(&harness), [Some(3_600), Some(2_400)]);
}

#[tokio::test]
async fn a_timeout_by_discord_that_ran_out_is_not_given_again() {
    let harness = harness();
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3_600));
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(timeouts_sent(&harness), [Some(3_600)]);
}

#[tokio::test]
async fn a_timeout_by_discord_is_capped_at_twenty_eight_days() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    harness
        .invoke(timeout(MEMBER, "40d", Some("spam")))
        .await
        .unwrap();
    harness.set_config(json!({ "timeout_method": "discord" }));
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(timeouts_sent(&harness), [Some(28 * 86_400_u32)]);
}

#[tokio::test]
async fn a_revoked_timeout_is_not_given_again() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness
        .invoke(case_revoke(1, Some("mistake")))
        .await
        .unwrap();
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(roles_added(&harness), [TIMEOUT_ROLE]);
}

#[tokio::test]
async fn a_kick_or_a_ban_is_not_given_again() {
    let harness = harness();
    harness.invoke(kick(MEMBER, Some("spam"))).await.unwrap();
    let before = harness.api().actions().len();
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(harness.api().actions().len(), before);

    harness
        .invoke(ban(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    let before = harness.api().actions().len();
    rejoin(&harness, MEMBER).await.unwrap();
    assert_eq!(harness.api().actions().len(), before);
}

#[tokio::test]
async fn a_member_without_cases_causes_no_action_and_no_case() {
    let harness = harness();
    harness.set_config(json!({
        "timeout_method": "role",
        "timeout_role": "30",
        "voice_mute_method": "role",
        "voice_mute_role": "31"
    }));
    rejoin(&harness, MEMBER).await.unwrap();
    assert!(harness.api().actions().is_empty());
    assert!(harness.cases().get(1).await.unwrap().is_none());
}

#[tokio::test]
async fn a_bot_is_not_given_anything_again() {
    let harness = harness();
    harness.set_config(json!({ "timeout_method": "role", "timeout_role": "30" }));
    harness
        .cases()
        .create(NewCase {
            kind: CaseKind::new("timeout").unwrap(),
            target: Id::new(BOT),
            moderator: Id::new(MODERATOR),
            reason: None,
            expires_at: Some(i64::MAX),
        })
        .await
        .unwrap();
    harness.emit(joined(BOT, true)).await.unwrap();
    assert!(harness.api().actions().is_empty());
}

#[tokio::test]
async fn a_missing_or_invalid_role_gives_nothing_and_no_error() {
    let harness = harness();
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness
        .invoke(voice_mute(MEMBER, None, Some("noise")))
        .await
        .unwrap();
    for config in [
        json!({ "timeout_method": "role", "voice_mute_method": "role" }),
        json!({
            "timeout_method": "role",
            "timeout_role": "0",
            "voice_mute_method": "role",
            "voice_mute_role": "nope"
        }),
    ] {
        harness.set_config(config);
        let before = harness.api().actions().len();
        rejoin(&harness, MEMBER).await.unwrap();
        assert_eq!(harness.api().actions().len(), before);
    }
}

#[tokio::test]
async fn a_failing_role_does_not_stop_the_voice_role_from_being_given_again() {
    let harness = harness();
    harness.set_config(json!({
        "timeout_method": "role",
        "timeout_role": "30",
        "voice_mute_method": "role",
        "voice_mute_role": "31"
    }));
    harness
        .invoke(timeout(MEMBER, "1h", Some("spam")))
        .await
        .unwrap();
    harness
        .invoke(voice_mute(MEMBER, None, Some("noise")))
        .await
        .unwrap();
    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("missing permission".to_owned()));
    let error = rejoin(&harness, MEMBER).await.unwrap_err();
    assert!(
        matches!(error, ModuleError::Discord(DiscordError::Forbidden(_))),
        "{error:?}"
    );
    assert_eq!(
        roles_added(&harness),
        [TIMEOUT_ROLE, VOICE_ROLE, VOICE_ROLE]
    );
}

#[test]
fn the_module_listens_for_joins_with_the_members_intent() {
    let manifest = module().unwrap().manifest().clone();
    assert!(manifest.intents.contains(Intents::GUILD_MEMBERS));
    assert!(manifest.subscription(EventKind::MemberJoined).is_some());
}

// ---- an undo button of an older punishment ----

/// Case #1 of the member is revoked by the lift #3, and case #2 still stands.
async fn assert_only_the_older_case_revoked(harness: &Harness) {
    let first = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        first.revoked.map(|revoked| revoked.reason),
        Some(Some("removed by case #3".to_owned()))
    );
    let second = harness.cases().get(2).await.unwrap().unwrap();
    assert!(second.revoked.is_none(), "{second:?}");
}

#[tokio::test]
async fn pressing_undo_on_an_older_timeout_revokes_only_that_one() {
    let harness = harness();
    harness
        .invoke(timeout(MEMBER, "1h", Some("first")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(timeout(MEMBER, "1h", Some("second")))
        .await
        .unwrap();

    press(&harness, &undo_id_of_case("timeout", MEMBER, 1)).await;
    assert_only_the_older_case_revoked(&harness).await;
}

#[tokio::test]
async fn pressing_undo_on_an_older_ban_revokes_only_that_one() {
    let harness = harness();
    harness
        .invoke(ban(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();

    press_as(
        &harness,
        &undo_id_of_case("ban", MEMBER, 1),
        MODERATOR,
        &[ADMIN_ROLE],
    )
    .await;
    assert_only_the_older_case_revoked(&harness).await;
}

#[tokio::test]
async fn pressing_undo_on_an_older_voice_mute_revokes_only_that_one() {
    let harness = harness();
    harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("loud")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(voice_mute(MEMBER, Some("1h"), Some("loud")))
        .await
        .unwrap();

    press(&harness, &undo_id_of_case("vcmute", MEMBER, 1)).await;
    assert_only_the_older_case_revoked(&harness).await;
}

// ---- durations as a date and time ----

const DAY_SECONDS: i64 = 86_400;

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// `unix` written as `YYYY-MM-DD HH:MM` in UTC, which an English server reads without a zone.
fn utc_text(unix: i64) -> String {
    let mut days = unix.div_euclid(DAY_SECONDS);
    let seconds = unix.rem_euclid(DAY_SECONDS);
    let mut year = 1970;
    while days >= if is_leap(year) { 366 } else { 365 } {
        days -= if is_leap(year) { 366 } else { 365 };
        year += 1;
    }
    let february = if is_leap(year) { 29 } else { 28 };
    let lengths = [31, february, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    while days >= lengths[month] {
        days -= lengths[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02} {:02}:{:02}",
        month + 1,
        days + 1,
        seconds / 3_600,
        seconds % 3_600 / 60
    )
}

/// The minute `days` from now, which [`utc_text`] names exactly.
fn minute_in(days: i64) -> i64 {
    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    (now + days * DAY_SECONDS).div_euclid(60) * 60
}

#[tokio::test]
async fn a_timeout_to_a_date_lasts_until_it() {
    let harness = harness();
    let target = minute_in(3);
    let replies = harness
        .invoke(timeout_of(MEMBER, Some(&utc_text(target)), Some("spam")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["timeout", "dm"]);
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.expires_at, Some(target));
    let seconds = u32::try_from(target - case.created_at).unwrap();
    let applied: Vec<u32> = harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::Timeout {
                duration_seconds: Some(seconds),
                ..
            } => Some(*seconds),
            _ => None,
        })
        .collect();
    assert_eq!(applied, [seconds]);
}

#[tokio::test]
async fn a_timed_ban_to_a_date_is_lifted_at_that_moment() {
    let harness = harness();
    let target = minute_in(3);
    let replies = harness
        .invoke(ban(MEMBER, Some("raid"), Some(&utc_text(target)), None))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(harness.jobs(), 1);
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.expires_at, Some(target));

    let until = u64::try_from(target - case.created_at).unwrap();
    harness.advance(Duration::from_secs(until - 1));
    assert!(harness.run_due_jobs().await.is_empty(), "not due yet");
    harness.advance(Duration::from_secs(1));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["dm", "ban", "unban"]);
}

#[tokio::test]
async fn a_voice_mute_to_a_date_expires_at_that_moment() {
    let harness = harness();
    let target = minute_in(2);
    let replies = harness
        .invoke(voice_mute(MEMBER, Some(&utc_text(target)), Some("loud")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.expires_at, Some(target));
}

#[tokio::test]
async fn a_date_that_has_passed_is_refused_before_anything_is_asked_of_discord() {
    let harness = harness();
    let past = minute_in(0) - 3_600;
    let replies = harness
        .invoke(timeout_of(MEMBER, Some(&utc_text(past)), Some("spam")))
        .await
        .unwrap();
    let shown = replies[0].embeds[0].description.clone().unwrap_or_default();
    assert!(shown.contains("passed"), "{shown:?}");
    // Only the answer itself is recorded: no lookup or change happens on Discord.
    assert!(
        harness
            .api()
            .actions()
            .iter()
            .all(|action| matches!(action, Action::Respond { .. })),
        "{:?}",
        harness.api().actions()
    );
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn a_date_more_than_twenty_eight_days_away_is_refused_for_a_discord_timeout() {
    let harness = harness();
    let replies = harness
        .invoke(timeout_of(
            MEMBER,
            Some(&utc_text(minute_in(40))),
            Some("spam"),
        ))
        .await
        .unwrap();
    assert!(text(&replies).contains("28 days"), "{replies:?}");
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}
