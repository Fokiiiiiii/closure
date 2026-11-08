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

/// A message with an author, a text, files and a pin, as the pretend Discord holds it.
fn posted(
    id: u64,
    author: u64,
    bot: bool,
    content: &str,
    files: usize,
    pinned: bool,
) -> twilight_model::channel::Message {
    serde_json::from_value(json!({
        "id": id.to_string(), "channel_id": CHANNEL.to_string(), "content": content,
        "author": { "id": author.to_string(), "username": "someone", "discriminator": "0",
                    "avatar": null, "bot": bot },
        "attachments": (0..files).map(|n| json!({
            "id": (900 + n).to_string(), "filename": "a.png", "size": 10,
            "url": "https://cdn.example/a.png", "proxy_url": "https://cdn.example/a.png"
        })).collect::<Vec<_>>(),
        "embeds": [], "mention_everyone": false, "mention_roles": [], "mentions": [],
        "pinned": pinned, "timestamp": "2026-10-24T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "type": 0,
    }))
    .unwrap()
}

fn deleted_ids(harness: &Harness) -> Vec<u64> {
    let mut ids = Vec::new();
    for action in harness.api().actions() {
        match action {
            Action::BulkDeleteMessages { message_ids, .. } => {
                ids.extend(message_ids.iter().map(|id| id.get()));
            }
            Action::DeleteMessage { message_id, .. } => ids.push(message_id.get()),
            _ => {}
        }
    }
    ids.sort_unstable();
    ids
}

/// Eight messages, oldest first, and the ids they got.
fn crowded() -> (Harness, Vec<u64>) {
    let harness = harness();
    let ids: Vec<u64> = (1..=8).map(|n| message_id(100 - n * 10)).collect();
    let put = |index: usize, author: u64, bot: bool, text: &str, files: usize| {
        harness
            .api()
            .put_channel_message(posted(ids[index], author, bot, text, files, false));
    };
    put(0, 7, false, "hello there", 0);
    put(1, 8, false, "see https://example.com now", 0);
    put(2, 7, false, "look", 1);
    put(3, 9, true, "I am a bot", 0);
    put(4, 7, false, "join discord.gg/abc", 0);
    put(5, 8, false, "HELLO again", 0);
    put(6, 9, true, "another bot", 1);
    put(7, 7, false, "last", 0);
    (harness, ids)
}

/// Which of the eight messages (by position, oldest first) a `/clear` with these options deletes.
async fn cleared(count: i64, options: &[(&str, CommandOptionValue)]) -> Vec<usize> {
    let (harness, ids) = crowded();
    let mut invocation = clear(count);
    for (name, value) in options {
        invocation = invocation.option(name, value.clone());
    }
    harness.invoke(invocation).await.unwrap();
    deleted_ids(&harness)
        .iter()
        .map(|id| ids.iter().position(|known| known == id).unwrap())
        .collect()
}

#[tokio::test]
async fn clear_takes_only_the_messages_that_fit_the_filters() {
    let yes = || CommandOptionValue::Boolean(true);
    let text = |text: &str| CommandOptionValue::String(text.to_owned());
    // Without a filter, the newest messages, whoever wrote them.
    assert_eq!(cleared(3, &[]).await, [5, 6, 7]);
    // A member.
    assert_eq!(
        cleared(8, &[("user", CommandOptionValue::User(Id::new(7)))]).await,
        [0, 2, 4, 7]
    );
    // Bots; links and invites; files; a text in any case.
    assert_eq!(cleared(8, &[("bots", yes())]).await, [3, 6]);
    assert_eq!(cleared(8, &[("links", yes())]).await, [1, 4]);
    assert_eq!(cleared(8, &[("files", yes())]).await, [2, 6]);
    assert_eq!(cleared(8, &[("contains", text("hello"))]).await, [0, 5]);
    // Filters hold together.
    assert_eq!(cleared(8, &[("bots", yes()), ("files", yes())]).await, [6]);
    // The count is how many that fit, the newest of them.
    assert_eq!(
        cleared(2, &[("user", CommandOptionValue::User(Id::new(7)))]).await,
        [4, 7]
    );
}

#[tokio::test]
async fn clear_leaves_pinned_messages_unless_they_are_asked_for() {
    let build = || {
        let harness = harness();
        let (kept, plain) = (message_id(20), message_id(10));
        harness
            .api()
            .put_channel_message(posted(kept, 7, false, "rules", 0, true));
        harness
            .api()
            .put_channel_message(posted(plain, 7, false, "chatter", 0, false));
        (harness, kept, plain)
    };
    let (harness, kept, plain) = build();
    harness.invoke(clear(5)).await.unwrap();
    assert_eq!(
        deleted_ids(&harness),
        [plain],
        "the pinned message stays ({kept})"
    );

    let (harness, kept, plain) = build();
    harness
        .invoke(clear(5).option("pinned", CommandOptionValue::Boolean(true)))
        .await
        .unwrap();
    assert_eq!(deleted_ids(&harness), [kept, plain]);
}

#[tokio::test]
async fn a_filter_looks_back_past_the_messages_that_do_not_fit_and_stops_at_a_limit() {
    let harness = harness();
    // One message of the member, long ago, below more than a page of others.
    let mine = message_id(1_000);
    harness
        .api()
        .put_channel_message(posted(mine, 7, false, "mine", 0, false));
    for n in 0..150_u64 {
        harness
            .api()
            .put_channel_message(posted(message_id(900 - n), 8, false, "noise", 0, false));
    }
    harness
        .invoke(clear(5).option("user", CommandOptionValue::User(Id::new(7))))
        .await
        .unwrap();
    assert_eq!(deleted_ids(&harness), [mine], "found on the second page");
}
