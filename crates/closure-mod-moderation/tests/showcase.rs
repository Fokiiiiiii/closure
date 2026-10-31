//! The showcase of the moderation module: what its main actions answer, with realistic names,
//! reasons and ids. `closure-xtask replay` posts exactly the messages of these tests (see
//! `docs/preview.md`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::event::CommandOptionValue;
use closure_mod_moderation::builder;
use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{Level, Locale};
use serde_json::json;
use twilight_model::guild::Role;
use twilight_model::id::Id;

/// The guild the harness runs its commands in.
const GUILD: u64 = 1;
const MODERATOR_ROLE: u64 = 1_100_000_000_000_000_001;
const ADMIN_ROLE: u64 = 1_100_000_000_000_000_002;
const MODERATOR: u64 = 298_765_432_109_876_543;
const TARGET: u64 = 512_345_678_901_234_567;

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
    harness
        .api()
        .set_not_a_member(Id::new(GUILD), Id::new(TARGET));
    harness.set_guild_locale(Locale::Ja);
    harness
}

fn user(id: u64) -> CommandOptionValue {
    CommandOptionValue::User(Id::new(id))
}

fn text(value: &str) -> CommandOptionValue {
    CommandOptionValue::String(value.to_owned())
}

/// The last reply of an action: its card, the case the log shows.
fn card(replies: &[Reply]) -> &Reply {
    replies.last().expect("the action answers")
}

#[tokio::test]
async fn showcase_warning_shows_its_case() {
    let h = harness();
    let invocation = Invocation::new("warn")
        .option("user", user(TARGET))
        .option("reason", text("spam links in #general"))
        .user(MODERATOR)
        .roles([MODERATOR_ROLE]);

    let replies = h.invoke(invocation).await.unwrap();

    assert_eq!(
        card(&replies).embeds[0].title.as_deref(),
        Some("ケース #1 · 警告")
    );
}

#[tokio::test]
async fn showcase_timeout_shows_its_case() {
    let h = harness();
    let invocation = Invocation::new("timeout")
        .option("user", user(TARGET))
        .option("duration", text("1h"))
        .option("reason", text("flooding the chat"))
        .user(MODERATOR)
        .roles([MODERATOR_ROLE]);

    let replies = h.invoke(invocation).await.unwrap();

    assert!(card(&replies).embeds[0].title.is_some());
}

#[tokio::test]
async fn showcase_kick_shows_its_case() {
    let h = harness();
    let invocation = Invocation::new("kick")
        .option("user", user(TARGET))
        .option("reason", text("repeated spam after a warning"))
        .user(MODERATOR)
        .roles([MODERATOR_ROLE]);

    let replies = h.invoke(invocation).await.unwrap();

    assert!(card(&replies).embeds[0].title.is_some());
}

#[tokio::test]
async fn showcase_ban_shows_its_case() {
    let h = harness();
    let invocation = Invocation::new("ban")
        .option("user", user(TARGET))
        .option("duration", text("7d"))
        .option("reason", text("raid: joined with twelve accounts"))
        .user(MODERATOR)
        .roles([ADMIN_ROLE]);

    let replies = h.invoke(invocation).await.unwrap();

    assert!(card(&replies).embeds[0].title.is_some());
}

#[tokio::test]
async fn showcase_note_shows_its_case() {
    let h = harness();
    let invocation = Invocation::new("note")
        .option("user", user(TARGET))
        .option(
            "text",
            text("asked about the rules twice; answered politely"),
        )
        .user(MODERATOR)
        .roles([MODERATOR_ROLE]);

    let replies = h.invoke(invocation).await.unwrap();

    assert!(!card(&replies).embeds.is_empty());
}

/// A command a moderator runs, with the moderator role.
fn moderator_runs(name: &str) -> Invocation {
    Invocation::new(name)
        .user(MODERATOR)
        .roles([MODERATOR_ROLE])
}

/// A command an admin runs, with the admin role.
fn admin_runs(name: &str) -> Invocation {
    Invocation::new(name).user(MODERATOR).roles([ADMIN_ROLE])
}

#[tokio::test]
async fn showcase_untimeout_shows_its_case() {
    let h = harness();
    h.invoke(
        moderator_runs("timeout")
            .option("user", user(TARGET))
            .option("duration", text("1h"))
            .option("reason", text("flooding the chat")),
    )
    .await
    .unwrap();

    let replies = h
        .invoke(
            moderator_runs("untimeout")
                .option("user", user(TARGET))
                .option("reason", text("apologised")),
        )
        .await
        .unwrap();

    assert!(card(&replies).embeds[0].description.is_some());
}

#[tokio::test]
async fn showcase_ban_for_good_shows_its_case() {
    let h = harness();
    let replies = h
        .invoke(
            admin_runs("ban")
                .option("user", user(TARGET))
                .option("reason", text("doxxing another member")),
        )
        .await
        .unwrap();

    assert!(card(&replies).embeds[0].title.is_some());
}

#[tokio::test]
async fn showcase_unban_shows_its_case() {
    let h = harness();
    h.invoke(
        admin_runs("ban")
            .option("user", user(TARGET))
            .option("duration", text("7d"))
            .option("reason", text("raid")),
    )
    .await
    .unwrap();

    let replies = h
        .invoke(
            admin_runs("unban")
                .option("user", user(TARGET))
                .option("reason", text("appeal accepted")),
        )
        .await
        .unwrap();

    assert!(card(&replies).embeds[0].description.is_some());
}

#[tokio::test]
async fn showcase_voice_mute_shows_its_case() {
    let h = harness();
    let replies = h
        .invoke(
            moderator_runs("vcmute")
                .option("user", user(TARGET))
                .option("duration", text("1h"))
                .option("reason", text("shouting over the stage")),
        )
        .await
        .unwrap();

    assert!(card(&replies).embeds[0].title.is_some());
}

#[tokio::test]
async fn showcase_voice_unmute_shows_its_case() {
    let h = harness();
    h.invoke(
        moderator_runs("vcmute")
            .option("user", user(TARGET))
            .option("reason", text("shouting over the stage")),
    )
    .await
    .unwrap();

    let replies = h
        .invoke(
            moderator_runs("vcunmute")
                .option("user", user(TARGET))
                .option("reason", text("calmed down")),
        )
        .await
        .unwrap();

    assert!(card(&replies).embeds[0].description.is_some());
}

#[tokio::test]
async fn showcase_warnings_lists_the_active_warnings() {
    let h = harness();
    h.invoke(
        moderator_runs("warn")
            .option("user", user(TARGET))
            .option("reason", text("spam links in #general")),
    )
    .await
    .unwrap();

    let replies = h
        .invoke(moderator_runs("warnings").option("user", user(TARGET)))
        .await
        .unwrap();

    assert!(!card(&replies).embeds.is_empty());
}

#[tokio::test]
async fn showcase_case_view_shows_the_case() {
    let h = harness();
    h.invoke(
        moderator_runs("warn")
            .option("user", user(TARGET))
            .option("reason", text("spam links in #general")),
    )
    .await
    .unwrap();

    let replies = h
        .invoke(
            moderator_runs("case")
                .subcommand("view")
                .option("number", CommandOptionValue::Integer(1)),
        )
        .await
        .unwrap();

    assert!(card(&replies).embeds[0].title.is_some());
}

#[tokio::test]
async fn showcase_case_list_shows_the_cases_of_a_member() {
    let h = harness();
    h.invoke(
        moderator_runs("warn")
            .option("user", user(TARGET))
            .option("reason", text("spam links in #general")),
    )
    .await
    .unwrap();

    let replies = h
        .invoke(
            moderator_runs("case")
                .subcommand("list")
                .option("user", user(TARGET)),
        )
        .await
        .unwrap();

    assert!(!card(&replies).embeds.is_empty());
}

#[tokio::test]
async fn showcase_case_revoke_shows_the_case() {
    let h = harness();
    h.invoke(
        moderator_runs("warn")
            .option("user", user(TARGET))
            .option("reason", text("spam links in #general")),
    )
    .await
    .unwrap();

    let replies = h
        .invoke(
            moderator_runs("case")
                .subcommand("revoke")
                .option("number", CommandOptionValue::Integer(1))
                .option("reason", text("appeal accepted")),
        )
        .await
        .unwrap();

    assert!(card(&replies).embeds[0].description.is_some());
}
