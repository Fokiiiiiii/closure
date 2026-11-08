use closure_core::action::Action;
use closure_core::event::{ChannelId, CommandOptionValue, Event, GuildId, UserId, UserInfo};
use closure_sdk::Level;
use closure_sdk::testing::{Harness, Invocation};
use twilight_model::id::Id;

use crate::builder;

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
    harness
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
