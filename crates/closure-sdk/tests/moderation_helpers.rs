//! The moderation helpers of the SDK, run through the Harness: durations as options and as
//! words, and the check of a target.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::CommandOptionValue;
use closure_sdk::duration::{self, DurationError, TimeStyle};
use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{Capability, Command, CommandArgs, Locale, ModuleBuilder, OptionKind, UserId};
use serde_json::json;
use twilight_model::guild::{Member, Role};
use twilight_model::id::Id;

#[derive(CommandArgs)]
struct Mute {
    /// Member to mute
    user: UserId,
    /// How long
    time: Duration,
}

#[derive(CommandArgs)]
struct Lapse {
    /// How long, if given
    time: Option<Duration>,
}

#[derive(CommandArgs)]
struct Span {
    /// Seconds
    seconds: i64,
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("mute", "Mutes", "0.1.0")
        .unwrap()
        .command(
            Command::<Mute>::new("mute", "Mute a member").handler(|cx, args| async move {
                cx.reply(format!("{} {}", args.user.get(), args.time.as_secs()))
                    .await
            }),
        )
        .command(
            Command::<Lapse>::new("lapse", "A duration").handler(|cx, args| async move {
                cx.reply(
                    args.time
                        .map_or("none".to_owned(), |time| time.as_secs().to_string()),
                )
                .await
            }),
        )
        .command(
            Command::<Span>::new("span", "Describe a length").handler(|cx, args| async move {
                let time = Duration::from_secs(u64::try_from(args.seconds).unwrap_or(0));
                cx.reply(cx.format_duration(time)).await
            }),
        )
}

async fn run(invocation: Invocation) -> Vec<Reply> {
    Harness::new(module())
        .unwrap()
        .invoke(invocation)
        .await
        .unwrap()
}

fn span(seconds: i64) -> Invocation {
    Invocation::new("span").option("seconds", CommandOptionValue::Integer(seconds))
}

#[tokio::test]
async fn a_duration_option_reads_the_typed_words() {
    let replies = run(Invocation::new("mute")
        .option("user", CommandOptionValue::User(Id::new(42)))
        .option("time", CommandOptionValue::String("1h30m".into())))
    .await;
    assert_eq!(replies[0].content, "42 5400");
}

#[tokio::test]
async fn a_duration_option_that_is_not_a_duration_is_refused() {
    let replies = run(Invocation::new("mute")
        .option("user", CommandOptionValue::User(Id::new(42)))
        .option("time", CommandOptionValue::String("banana".into())))
    .await;
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0].content,
        "The value of `time` is missing or not valid."
    );
}

#[tokio::test]
async fn an_optional_duration_may_be_left_out() {
    let replies = run(Invocation::new("lapse")).await;
    assert_eq!(replies[0].content, "none");
}

#[test]
fn a_duration_option_is_published_as_a_string() {
    let option = Mute::options()
        .into_iter()
        .find(|option| option.name == "time")
        .unwrap();
    assert!(matches!(option.kind, OptionKind::String));
}

#[tokio::test]
async fn format_duration_in_english() {
    let cases = [
        (183_600, "2 days 3 hours"),
        (5400, "1 hour 30 minutes"),
        (86_400, "1 day"),
        (30, "30 seconds"),
        (1, "1 second"),
        (0, "less than a second"),
        (273_600, "3 days 4 hours"),
        (277_500, "3 days 5 hours"),
        (315_360_000, "3650 days"),
    ];
    for (seconds, expected) in cases {
        let replies = run(span(seconds)).await;
        assert_eq!(replies[0].content, expected, "{seconds} seconds");
    }
}

#[tokio::test]
async fn format_duration_in_japanese() {
    let cases = [
        (183_600, "2日 3時間"),
        (5400, "1時間 30分"),
        (86_400, "1日"),
        (30, "30秒"),
        (0, "1秒未満"),
        (273_600, "3日 4時間"),
        (277_500, "3日 5時間"),
        (315_360_000, "3650日"),
    ];
    for (seconds, expected) in cases {
        let replies = run(span(seconds).locale(Locale::Ja)).await;
        assert_eq!(replies[0].content, expected, "{seconds} seconds");
    }
}

#[test]
fn timestamp_is_discords_markup() {
    assert_eq!(
        duration::timestamp(1_700_000_000, TimeStyle::Relative),
        "<t:1700000000:R>"
    );
    assert_eq!(
        DurationError::TooLarge,
        duration::parse("3651d").unwrap_err()
    );
}

#[derive(CommandArgs)]
struct Act {
    /// Member to act on
    user: UserId,
}

/// A module whose only command checks the target. It has the capability to read the guild
/// unless `read_guild` is false.
fn guarded(read_guild: bool) -> Harness {
    let mut builder = ModuleBuilder::new("act", "Acts", "0.1.0").unwrap().command(
        Command::<Act>::new("act", "Act on a member").handler(|cx, args| async move {
            cx.check_target(args.user).await?;
            cx.reply("ok").await
        }),
    );
    if read_guild {
        builder = builder.capability(Capability::ReadGuild);
    }
    Harness::new(builder).unwrap()
}

/// The server, with roles at positions: 1 at 5, 2 at 3, 3 at 5, 4 at 9 and 5 at 1.
fn server() -> Harness {
    let harness = guarded(true);
    harness.api().set_roles(
        Id::new(1),
        vec![role(1, 5), role(2, 3), role(3, 5), role(4, 9), role(5, 1)],
    );
    harness
}

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

/// What the module answers when `by` acts on the member `target`.
async fn act(harness: &Harness, by: Invocation, target: u64) -> String {
    let replies = harness
        .invoke(by.option("user", CommandOptionValue::User(Id::new(target))))
        .await
        .unwrap();
    replies[0].content.clone()
}

const SELF_TEXT: &str = "You cannot do that to yourself.";
const OWNER_TEXT: &str = "You cannot do that to the owner of the server.";
const BOT_TEXT: &str = "You cannot do that to a bot.";
const HIGHER_TEXT: &str = "You cannot do that to someone whose highest role is not below yours.";

#[tokio::test]
async fn nobody_can_act_on_themselves() {
    let harness = guarded(true);
    assert_eq!(act(&harness, Invocation::new("act"), 100).await, SELF_TEXT);
}

#[tokio::test]
async fn the_refusal_is_in_the_users_language() {
    let harness = guarded(true);
    assert_eq!(
        act(&harness, Invocation::new("act").locale(Locale::Ja), 100).await,
        "自分自身には実行できません。"
    );
}

#[tokio::test]
async fn the_owner_of_the_server_cannot_be_acted_on() {
    let harness = guarded(true);
    assert_eq!(
        act(&harness, Invocation::new("act"), 999_999_999).await,
        OWNER_TEXT
    );
}

#[tokio::test]
async fn a_bot_cannot_be_acted_on() {
    let harness = guarded(true);
    harness.api().set_member(Id::new(1), member(200, &[], true));
    assert_eq!(act(&harness, Invocation::new("act"), 200).await, BOT_TEXT);
}

#[tokio::test]
async fn someone_who_is_not_a_member_passes_the_check() {
    let harness = guarded(true);
    harness.api().set_not_a_member(Id::new(1), Id::new(300));
    assert_eq!(act(&harness, Invocation::new("act"), 300).await, "ok");
}

#[tokio::test]
async fn a_member_below_the_top_role_of_the_actor_passes() {
    let harness = server();
    harness
        .api()
        .set_member(Id::new(1), member(201, &[2], false));
    assert_eq!(
        act(&harness, Invocation::new("act").roles([1]), 201).await,
        "ok"
    );
}

#[tokio::test]
async fn a_member_with_the_same_top_position_is_refused() {
    let harness = server();
    harness
        .api()
        .set_member(Id::new(1), member(202, &[3], false));
    assert_eq!(
        act(&harness, Invocation::new("act").roles([1]), 202).await,
        HIGHER_TEXT
    );
}

#[tokio::test]
async fn a_member_with_a_higher_role_is_refused() {
    let harness = server();
    harness
        .api()
        .set_member(Id::new(1), member(203, &[4], false));
    assert_eq!(
        act(&harness, Invocation::new("act").roles([1]), 203).await,
        HIGHER_TEXT
    );
}

#[tokio::test]
async fn the_owner_of_the_server_may_act_on_a_higher_role() {
    let harness = server();
    harness
        .api()
        .set_member(Id::new(1), member(203, &[4], false));
    assert_eq!(
        act(&harness, Invocation::new("act").roles([1]).owner(), 203).await,
        "ok"
    );
}

#[tokio::test]
async fn without_roles_on_either_side_the_positions_tie() {
    let harness = server();
    harness
        .api()
        .set_member(Id::new(1), member(204, &[], false));
    assert_eq!(
        act(&harness, Invocation::new("act"), 204).await,
        HIGHER_TEXT
    );
}

#[tokio::test]
async fn a_role_of_the_target_outranks_a_actor_without_roles() {
    let harness = server();
    harness
        .api()
        .set_member(Id::new(1), member(205, &[5], false));
    assert_eq!(
        act(&harness, Invocation::new("act"), 205).await,
        HIGHER_TEXT
    );
}

#[tokio::test]
async fn a_target_without_roles_is_below_an_actor_with_one() {
    let harness = server();
    harness
        .api()
        .set_member(Id::new(1), member(206, &[], false));
    assert_eq!(
        act(&harness, Invocation::new("act").roles([1]), 206).await,
        "ok"
    );
}

#[tokio::test]
async fn checking_a_target_needs_the_read_guild_capability() {
    let harness = guarded(false);
    let error = harness
        .invoke(Invocation::new("act").option("user", CommandOptionValue::User(Id::new(42))))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ModuleError::Discord(DiscordError::CapabilityDenied(Capability::ReadGuild))
    ));
}
