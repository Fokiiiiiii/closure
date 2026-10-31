//! `/clear`, run against the SDK's pretend Discord.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{SystemTime, UNIX_EPOCH};

use closure_core::action::Action;
use closure_core::event::CommandOptionValue;
use closure_mod_moderation::builder;
use closure_sdk::Level;
use closure_sdk::testing::{Harness, Invocation};
use serde_json::json;
use twilight_model::guild::Role;
use twilight_model::id::Id;

const GUILD: u64 = 1;
const CHANNEL: u64 = 5;
const MODERATOR: u64 = 100;
const MODERATOR_ROLE: u64 = 10;
const DAY: u64 = 86_400;
/// Discord's epoch, in milliseconds since the Unix epoch: a snowflake counts from it.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

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

/// The id a message sent `seconds_ago` gets, so the module can tell how old it is.
fn message_id(seconds_ago: u64) -> u64 {
    let now_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    (now_ms - DISCORD_EPOCH_MS - seconds_ago * 1000) << 22
}

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR_ROLE), Level::MODERATOR);
    });
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(MODERATOR_ROLE, 5)]);
    harness
        .api()
        .add_channel(Id::new(CHANNEL), Some(Id::new(GUILD)));
    harness
}

fn clear(count: i64) -> Invocation {
    Invocation::new("clear")
        .channel(CHANNEL)
        .roles([MODERATOR_ROLE])
        .user(MODERATOR)
        .option("count", CommandOptionValue::Integer(count))
}

fn bulk_deleted(harness: &Harness) -> Option<Vec<Id<twilight_model::id::marker::MessageMarker>>> {
    harness
        .api()
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::BulkDeleteMessages { message_ids, .. } => Some(message_ids),
            _ => None,
        })
}

fn deletes_anything(harness: &Harness) -> bool {
    harness.api().actions().iter().any(|action| {
        matches!(
            action,
            Action::DeleteMessage { .. } | Action::BulkDeleteMessages { .. }
        )
    })
}

#[tokio::test]
async fn clear_deletes_the_newest_messages_in_one_bulk_delete() {
    let harness = harness();
    let (oldest, middle, newest) = (message_id(30), message_id(20), message_id(10));
    for id in [oldest, middle, newest] {
        harness.api().add_channel_message(Id::new(CHANNEL), id);
    }

    harness.invoke(clear(2)).await.unwrap();

    assert_eq!(
        bulk_deleted(&harness),
        Some(vec![Id::new(newest), Id::new(middle)]),
        "{:?}",
        harness.api().actions()
    );
}

#[tokio::test]
async fn clear_deletes_a_lone_message_on_its_own() {
    let harness = harness();
    let only = message_id(10);
    harness.api().add_channel_message(Id::new(CHANNEL), only);

    harness.invoke(clear(1)).await.unwrap();

    assert!(
        harness.api().actions().iter().any(|action| matches!(
            action,
            Action::DeleteMessage { message_id, .. } if message_id.get() == only
        )),
        "{:?}",
        harness.api().actions()
    );
    assert!(bulk_deleted(&harness).is_none());
}

#[tokio::test]
async fn clear_leaves_the_messages_older_than_fourteen_days_and_counts_them() {
    let harness = harness();
    let old = message_id(15 * DAY);
    let fresh = message_id(10);
    harness.api().add_channel_message(Id::new(CHANNEL), old);
    harness.api().add_channel_message(Id::new(CHANNEL), fresh);

    let replies = harness.invoke(clear(2)).await.unwrap();

    assert!(bulk_deleted(&harness).is_none());
    assert!(
        harness.api().actions().iter().any(|action| matches!(
            action,
            Action::DeleteMessage { message_id, .. } if message_id.get() == fresh
        )),
        "{:?}",
        harness.api().actions()
    );
    let reply = &replies.last().unwrap().content;
    assert!(reply.contains("Deleted 1 messages"), "{reply}");
    assert!(reply.contains("1 older than fourteen days"), "{reply}");
}

#[tokio::test]
async fn clear_refuses_a_count_outside_one_to_one_hundred() {
    for count in [0, 101] {
        let harness = harness();
        harness
            .api()
            .add_channel_message(Id::new(CHANNEL), message_id(10));

        let replies = harness.invoke(clear(count)).await.unwrap();

        assert!(!deletes_anything(&harness), "count {count}");
        let reply = &replies.last().unwrap().content;
        assert!(reply.contains("1 to 100"), "count {count}: {reply}");
    }
}

#[tokio::test]
async fn clear_in_an_empty_channel_deletes_nothing() {
    let harness = harness();

    let replies = harness.invoke(clear(5)).await.unwrap();

    assert!(!deletes_anything(&harness));
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Deleted 0 messages")
    );
}
