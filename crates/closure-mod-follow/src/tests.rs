use std::time::Duration;

use closure_core::action::Action;
use closure_core::event::{ChannelId, CommandOptionValue, Event, GuildId, UserId, UserInfo};
use closure_sdk::Level;
use closure_sdk::testing::{Harness, Invocation};
use twilight_model::id::Id;

use crate::{MAX_NOTICE_ENTRIES, builder, claim_notice};

const MODERATOR_ROLE: u64 = 300;
const MODERATOR: u64 = 7;
const MEMBER: u64 = 20;
const LOBBY: u64 = 50;
const ROOM: u64 = 51;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR_ROLE), Level::MODERATOR);
    });
    declare(&harness, MODERATOR, &[MODERATOR_ROLE]);
    harness
}

/// Makes the pretend Discord know `user` in the test guild, with these roles.
fn declare(harness: &Harness, user: u64, roles: &[u64]) {
    harness
        .api()
        .set_member(GuildId::new(1), member(user, roles));
}

fn member(user: u64, roles: &[u64]) -> twilight_model::guild::Member {
    serde_json::from_value(serde_json::json!({
        "user": {"id": user.to_string(), "username": "member", "discriminator": "0",
                 "avatar": null, "bot": false},
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "joined_at": "2020-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
    }))
    .unwrap()
}

fn runs(sub: &str, user: Option<u64>) -> Invocation {
    let invocation = Invocation::new("follow")
        .subcommand(sub)
        .user(MODERATOR)
        .roles([MODERATOR_ROLE]);
    match user {
        Some(user) => invocation.option("user", CommandOptionValue::User(Id::new(user))),
        None => invocation,
    }
}

/// A moderator with the moderator role asks to follow `target`.
fn adds(moderator: u64, target: u64) -> Invocation {
    Invocation::new("follow")
        .subcommand("add")
        .user(moderator)
        .roles([MODERATOR_ROLE])
        .option("user", CommandOptionValue::User(Id::new(target)))
}

fn voice(user: u64, bot: bool, before: Option<u64>, after: Option<u64>) -> Event {
    Event::VoiceStateChanged {
        guild_id: GuildId::new(1),
        user: UserInfo {
            id: UserId::new(user),
            name: "member".into(),
            bot,
        },
        before: before.map(ChannelId::new),
        after: after.map(ChannelId::new),
    }
}

fn told(harness: &Harness) -> Vec<(u64, String)> {
    harness
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SendDirectMessage {
                user_id, content, ..
            } => Some((user_id.get(), content.clone().unwrap_or_default())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_follower_is_told_when_the_member_joins_or_moves_but_not_when_they_leave() {
    let harness = harness();
    let replies = harness.invoke(runs("add", Some(MEMBER))).await.unwrap();
    assert!(replies.last().unwrap().content.contains("direct message"));

    harness
        .emit(voice(MEMBER, false, None, Some(LOBBY)))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(11));
    harness
        .emit(voice(MEMBER, false, Some(LOBBY), Some(ROOM)))
        .await
        .unwrap();
    harness
        .emit(voice(MEMBER, false, Some(ROOM), Some(ROOM)))
        .await
        .unwrap();
    harness
        .emit(voice(MEMBER, false, Some(ROOM), None))
        .await
        .unwrap();
    let messages = told(&harness);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages.iter().all(|(to, _)| *to == MODERATOR));
    assert!(messages[0].1.contains(&format!("<#{LOBBY}>")));
    assert!(messages[1].1.contains(&format!("<#{ROOM}>")));
}

#[tokio::test]
async fn a_member_who_moves_again_within_ten_seconds_is_told_once() {
    let harness = harness();
    harness.invoke(runs("add", Some(MEMBER))).await.unwrap();
    harness
        .emit(voice(MEMBER, false, None, Some(LOBBY)))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .emit(voice(MEMBER, false, Some(LOBBY), Some(ROOM)))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .emit(voice(MEMBER, false, Some(ROOM), Some(LOBBY)))
        .await
        .unwrap();
    assert_eq!(told(&harness).len(), 1);

    harness.advance(Duration::from_secs(11));
    harness
        .emit(voice(MEMBER, false, Some(LOBBY), Some(ROOM)))
        .await
        .unwrap();
    assert_eq!(told(&harness).len(), 2);
}

#[tokio::test]
async fn nobody_is_told_about_a_member_nobody_follows_or_after_the_follow_ends() {
    let harness = harness();
    harness
        .emit(voice(MEMBER, false, None, Some(LOBBY)))
        .await
        .unwrap();
    harness.invoke(runs("add", Some(MEMBER))).await.unwrap();
    harness.invoke(runs("remove", Some(MEMBER))).await.unwrap();
    harness
        .emit(voice(MEMBER, false, None, Some(LOBBY)))
        .await
        .unwrap();
    assert!(told(&harness).is_empty());
}

#[tokio::test]
async fn a_moderator_who_lost_the_permission_is_not_told_and_the_follow_ends() {
    let harness = harness();
    harness.invoke(runs("add", Some(MEMBER))).await.unwrap();
    declare(&harness, MODERATOR, &[]);
    harness
        .emit(voice(MEMBER, false, None, Some(LOBBY)))
        .await
        .unwrap();
    assert!(told(&harness).is_empty());

    declare(&harness, MODERATOR, &[MODERATOR_ROLE]);
    harness.advance(Duration::from_secs(60));
    harness
        .emit(voice(MEMBER, false, None, Some(LOBBY)))
        .await
        .unwrap();
    assert!(
        told(&harness).is_empty(),
        "the follow ended with the permission"
    );
}

#[tokio::test]
async fn a_moderator_who_lost_the_permission_can_still_stop_following() {
    let harness = harness();
    harness.invoke(runs("add", Some(MEMBER))).await.unwrap();
    declare(&harness, MODERATOR, &[]);
    let invocation = Invocation::new("follow")
        .subcommand("remove")
        .user(MODERATOR)
        .option("user", CommandOptionValue::User(Id::new(MEMBER)));
    let replies = harness.invoke(invocation).await.unwrap();
    assert!(replies.last().unwrap().content.contains("no longer follow"));
}

#[tokio::test]
async fn a_member_has_at_most_25_followers_and_the_26th_is_refused() {
    let harness = harness();
    for moderator in 100..126 {
        declare(&harness, moderator, &[MODERATOR_ROLE]);
        // The 26th is refused; its reply is not what this test checks.
        let _ = harness.invoke(adds(moderator, MEMBER)).await;
    }
    assert_eq!(
        harness
            .store()
            .set_count(&format!("follow:{MEMBER}"))
            .await
            .unwrap(),
        25
    );
    harness
        .emit(voice(MEMBER, false, None, Some(LOBBY)))
        .await
        .unwrap();
    let messages = told(&harness);
    assert_eq!(messages.len(), 25, "{messages:?}");
    assert!(messages.iter().all(|(to, _)| *to != 125));
}

#[tokio::test]
async fn a_moderator_follows_at_most_50_members_and_the_51st_is_refused() {
    let harness = harness();
    for target in 1000..1051 {
        let _ = harness.invoke(adds(MODERATOR, target)).await;
    }
    assert_eq!(
        harness
            .store()
            .set_count(&format!("following:{MODERATOR}"))
            .await
            .unwrap(),
        50
    );
    let replies = harness.invoke(runs("list", None)).await.unwrap();
    assert_eq!(replies.last().unwrap().content.matches("<@").count(), 50);
}

#[tokio::test]
async fn a_follow_that_only_the_moderator_index_has_is_dropped_by_the_list() {
    let harness = harness();
    harness
        .store()
        .set_add(&format!("following:{MODERATOR}"), MEMBER)
        .await
        .unwrap();
    let replies = harness.invoke(runs("list", None)).await.unwrap();
    assert!(replies.last().unwrap().content.contains("nobody"));
    assert_eq!(
        harness
            .store()
            .set_count(&format!("following:{MODERATOR}"))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn the_list_shows_who_a_moderator_follows() {
    let harness = harness();
    let none = harness.invoke(runs("list", None)).await.unwrap();
    assert!(none.last().unwrap().content.contains("nobody"));
    harness.invoke(runs("add", Some(MEMBER))).await.unwrap();
    let some = harness.invoke(runs("list", None)).await.unwrap();
    assert!(
        some.last()
            .unwrap()
            .content
            .contains(&format!("<@{MEMBER}>"))
    );
}

#[test]
fn the_notice_table_is_bounded() {
    let notices = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    for member in 0..(MAX_NOTICE_ENTRIES as u64 + 500) {
        assert!(claim_notice(&notices, member, 1_000));
    }
    assert!(notices.lock().unwrap().len() <= MAX_NOTICE_ENTRIES);
}
