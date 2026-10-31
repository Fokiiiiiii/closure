//! The showcase of the moderation module: what its main actions answer, with realistic names,
//! reasons and ids. `closure-xtask replay` posts exactly the messages of these tests (see
//! `docs/preview.md`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::event::CommandOptionValue;
use closure_mod_moderation::builder;
use closure_sdk::Level;
use closure_sdk::testing::{Harness, Invocation, Reply};
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
async fn a_warning_shows_its_case() {
    let h = harness();
    let invocation = Invocation::new("warn")
        .option("user", user(TARGET))
        .option("reason", text("spam links in #general"))
        .user(MODERATOR)
        .roles([MODERATOR_ROLE]);

    let replies = h.invoke(invocation).await.unwrap();

    assert!(card(&replies).embeds[0].title.is_some());
}

#[tokio::test]
async fn a_timeout_shows_its_case() {
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
async fn a_kick_shows_its_case() {
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
async fn a_ban_shows_its_case() {
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
async fn a_note_shows_its_case() {
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
