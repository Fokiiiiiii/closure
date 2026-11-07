use closure_core::action::Action;
use closure_core::event::{ChannelId, Event, GuildId, MemberInfo, RoleId, UserId, UserInfo};
use closure_sdk::testing::{ContextUse, Harness, Invocation};
use closure_sdk::{CommandOptionValue, Level};
use twilight_model::guild::Member;
use twilight_model::id::Id;

use crate::builder;

const MODERATOR: u64 = 200;

fn info(user: u64, nick: Option<&str>) -> MemberInfo {
    MemberInfo {
        guild_id: GuildId::new(1),
        user: UserInfo {
            id: UserId::new(user),
            name: format!("user-{user}"),
            bot: false,
        },
        roles: vec![RoleId::new(10)],
        nick: nick.map(str::to_owned),
        joined_at: None,
        timed_out_until: None,
    }
}

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness
        .api()
        .add_channel(ChannelId::new(30), Some(GuildId::new(1)));
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR), Level::MODERATOR);
    });
    harness.api().set_owner(GuildId::new(1), UserId::new(999));
    harness.api().set_roles(
        GuildId::new(1),
        vec![everyone_with("1024"), role_at(MODERATOR, 5)],
    );
    harness
}

fn role_at(id: u64, position: i64) -> twilight_model::guild::Role {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(), "name": format!("role-{id}"), "color": 0,
        "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "hoist": false, "managed": false, "mentionable": false,
        "permissions": "0", "position": position, "flags": 0,
    }))
    .unwrap()
}

fn user_option(id: u64) -> CommandOptionValue {
    CommandOptionValue::User(Id::new(id))
}

#[tokio::test]
async fn the_names_a_member_went_by_are_listed() {
    let harness = harness();
    harness
        .emit(Event::MemberUpdated(info(7, Some("First"))))
        .await
        .unwrap();
    harness
        .emit(Event::MemberUpdated(info(7, Some("Second"))))
        .await
        .unwrap();
    // The same names again add nothing.
    harness
        .emit(Event::MemberUpdated(info(7, Some("Second"))))
        .await
        .unwrap();
    let names = Invocation::new("names")
        .option("user", user_option(7))
        .user(50);
    let reply = harness.invoke(names).await.unwrap();
    let text = &reply[0].content;
    assert!(text.contains("First") && text.contains("Second") && text.contains("user-7"));
    assert_eq!(text.matches("Second").count(), 1);
    let nobody = Invocation::new("names")
        .option("user", user_option(8))
        .user(50);
    let reply = harness.invoke(nobody).await.unwrap();
    assert!(reply[0].content.contains("No other names"));
}

#[tokio::test]
async fn a_member_is_found_where_they_last_went_in_voice() {
    let harness = harness();
    let voice = |before: Option<u64>, after: Option<u64>| Event::VoiceStateChanged {
        guild_id: GuildId::new(1),
        user: info(7, None).user,
        before: before.map(ChannelId::new),
        after: after.map(ChannelId::new),
    };
    let locate = || {
        Invocation::new("locate")
            .option("user", user_option(7))
            .user(50)
    };
    harness.emit(voice(None, Some(30))).await.unwrap();
    assert!(
        harness.invoke(locate()).await.unwrap()[0]
            .content
            .contains("<#30>")
    );
    harness.emit(voice(Some(30), Some(31))).await.unwrap();
    assert!(
        harness.invoke(locate()).await.unwrap()[0]
            .content
            .contains("<#31>")
    );
    harness.emit(voice(Some(31), None)).await.unwrap();
    assert!(
        harness.invoke(locate()).await.unwrap()[0]
            .content
            .contains("not in a voice")
    );
}

#[tokio::test]
async fn only_moderators_move_and_disconnect_members() {
    let harness = harness();
    harness.api().set_member(Id::new(1), member_of(7));
    let moved = |h: &Harness| {
        h.actions().iter().any(|a| {
            matches!(a, Action::MoveMember { user_id, channel_id, .. }
                if user_id.get() == 7 && channel_id.get() == 30)
        })
    };
    let attempt = |roles: &[u64]| {
        Invocation::new("vcmove")
            .option("user", user_option(7))
            .option("channel", CommandOptionValue::Channel(Id::new(30)))
            .roles(roles.iter().copied())
            .user(50)
    };
    let _ = harness.invoke(attempt(&[])).await;
    assert!(!moved(&harness));
    harness.invoke(attempt(&[MODERATOR])).await.unwrap();
    assert!(moved(&harness));
    let kick = Invocation::new("vcmove")
        .option("user", user_option(7))
        .roles([MODERATOR])
        .user(50);
    harness.invoke(kick).await.unwrap();
    assert!(
        harness
            .actions()
            .iter()
            .any(|a| matches!(a, Action::DisconnectMember { user_id, .. } if user_id.get() == 7))
    );
}

fn twilight_member(user: u64, nick: &str) -> Member {
    serde_json::from_value(serde_json::json!({
        "user": {"id": user.to_string(), "username": format!("user-{user}"), "discriminator": "0",
                 "avatar": "abcdef0123456789abcdef0123456789", "global_name": null},
        "roles": ["10"], "nick": nick, "joined_at": "2024-01-02T03:04:05.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
    }))
    .unwrap()
}

#[tokio::test]
async fn a_member_card_and_the_avatar_come_from_the_member() {
    let harness = harness();
    harness
        .api()
        .set_member(GuildId::new(1), twilight_member(7, "Nicky"));
    let avatar = Invocation::new("avatar")
        .option("user", user_option(7))
        .user(50);
    let reply = harness.invoke(avatar).await.unwrap();
    assert!(
        reply[0].content.starts_with(
            "https://cdn.discordapp.com/avatars/7/abcdef0123456789abcdef0123456789.png"
        )
    );
    let menu = harness
        .context_command(ContextUse::on_user("Info", 7).user(50))
        .await
        .unwrap();
    let text = format!("{:?}", menu[0].embeds);
    assert!(text.contains("user-7") && text.contains("Nicky") && text.contains("<@&10>"));
}

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(
        closure_i18n::missing_keys(crate::EN, crate::JA).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        closure_i18n::missing_keys(crate::JA, crate::EN).unwrap(),
        Vec::<String>::new()
    );
}

fn text_channel(id: u64) -> twilight_model::channel::Channel {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(), "type": 0, "guild_id": "1",
        "name": format!("channel-{id}"), "permission_overwrites": [],
    }))
    .unwrap()
}

fn member_of(user: u64) -> Member {
    serde_json::from_value(serde_json::json!({
        "user": {"id": user.to_string(), "username": "member", "discriminator": "0",
                 "avatar": null, "bot": false},
        "roles": [], "joined_at": "2020-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
    }))
    .unwrap()
}

fn everyone_with(permissions: &str) -> twilight_model::guild::Role {
    serde_json::from_value(serde_json::json!({
        "id": "1", "name": "@everyone", "color": 0,
        "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "hoist": false, "managed": false, "mentionable": false,
        "permissions": permissions, "position": 0, "flags": 0,
    }))
    .unwrap()
}

#[tokio::test]
async fn a_channel_the_invoker_cannot_see_is_not_described() {
    let channelinfo = || {
        Invocation::new("info")
            .option("channel", CommandOptionValue::Channel(Id::new(30)))
            .user(50)
    };
    // Everybody may see it: it is described.
    let open = harness();
    open.api().set_channels(Id::new(1), vec![text_channel(30)]);
    open.api()
        .set_roles(Id::new(1), vec![everyone_with("1024")]);
    let shown = open.invoke(channelinfo()).await.unwrap();
    assert_eq!(shown[0].embeds[0].title.as_deref(), Some("channel-30"));
    // Nobody may: the answer is the same as for a channel that is not there.
    let hidden = harness();
    hidden
        .api()
        .set_channels(Id::new(1), vec![text_channel(30)]);
    hidden.api().set_roles(Id::new(1), vec![everyone_with("0")]);
    let refused = hidden.invoke(channelinfo()).await.unwrap();
    assert!(
        refused[0].embeds[0].title.is_none() && !refused[0].content.contains("channel-30"),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_thread_takes_the_view_of_its_parent_and_not_its_own_empty_overwrites() {
    let channelinfo = || {
        Invocation::new("info")
            .option("channel", CommandOptionValue::Channel(Id::new(31)))
            .user(50)
    };
    let thread: twilight_model::channel::Channel = serde_json::from_value(serde_json::json!({
        "id": "31", "type": 11, "guild_id": "1", "name": "secret-thread",
        "parent_id": "30", "permission_overwrites": [],
    }))
    .unwrap();
    let parent_that_hides: twilight_model::channel::Channel =
        serde_json::from_value(serde_json::json!({
            "id": "30", "type": 0, "guild_id": "1", "name": "hidden",
            "permission_overwrites": [{ "id": "1", "type": 0, "allow": "0", "deny": "1024" }],
        }))
        .unwrap();
    let hidden = harness();
    hidden
        .api()
        .set_channels(Id::new(1), vec![parent_that_hides, thread.clone()]);
    hidden
        .api()
        .set_roles(Id::new(1), vec![everyone_with("1024")]);
    let refused = hidden.invoke(channelinfo()).await.unwrap();
    assert!(
        refused[0].embeds.is_empty() || refused[0].embeds[0].title.is_none(),
        "{refused:?}"
    );
    assert!(!refused[0].content.contains("secret-thread"), "{refused:?}");
    // With a parent everybody sees, the thread is described.
    let open = harness();
    open.api()
        .set_channels(Id::new(1), vec![text_channel(30), thread]);
    open.api()
        .set_roles(Id::new(1), vec![everyone_with("1024")]);
    let shown = open.invoke(channelinfo()).await.unwrap();
    assert_eq!(shown[0].embeds[0].title.as_deref(), Some("secret-thread"));
}

#[tokio::test]
async fn members_are_found_by_a_name_they_went_by_and_only_by_a_moderator() {
    let harness = harness();
    for (id, nick) in [(7, "Old Name"), (8, "old name"), (9, "Other")] {
        harness
            .emit(Event::MemberUpdated(info(id, Some(nick))))
            .await
            .unwrap();
    }
    harness
        .emit(Event::MemberUpdated(info(7, Some("New Name"))))
        .await
        .unwrap();
    let find = |name: &str| {
        Invocation::new("names")
            .option("name", CommandOptionValue::String(name.to_owned()))
            .roles([MODERATOR])
            .user(50)
    };
    let reply = harness.invoke(find("OLD NAME")).await.unwrap();
    let text = &reply[0].content;
    assert!(text.contains("<@7>") && text.contains("<@8>"), "{text}");
    assert!(!text.contains("<@9>"), "{text}");
    let none = harness.invoke(find("never used")).await.unwrap();
    assert!(none[0].content.contains("No member is known"));
    let plain = Invocation::new("names")
        .option("name", CommandOptionValue::String("old name".to_owned()))
        .user(51);
    let refused = harness.invoke(plain).await.unwrap();
    assert!(!refused[0].content.contains("<@7>"), "{refused:?}");
}

fn voice_move(user: u64, before: Option<u64>, after: Option<u64>) -> Event {
    Event::VoiceStateChanged {
        guild_id: GuildId::new(1),
        user: info(user, None).user,
        before: before.map(ChannelId::new),
        after: after.map(ChannelId::new),
    }
}

fn disconnected(harness: &Harness, user: u64) -> usize {
    harness
        .actions()
        .iter()
        .filter(|a| matches!(a, Action::DisconnectMember { user_id, .. } if user_id.get() == user))
        .count()
}

#[tokio::test]
async fn everyone_in_a_voice_channel_is_moved_at_once_and_only_those_still_there() {
    let harness = harness();
    harness
        .api()
        .add_channel(ChannelId::new(32), Some(GuildId::new(1)));
    for user in [7, 8, 9] {
        harness.api().set_member(Id::new(1), member_of(user));
        harness
            .emit(voice_move(user, None, Some(30)))
            .await
            .unwrap();
    }
    // 9 left for another channel; 8 stays in 30.
    harness
        .emit(voice_move(9, Some(30), Some(31)))
        .await
        .unwrap();
    let all = Invocation::new("vcmove")
        .option("from", CommandOptionValue::Channel(Id::new(30)))
        .option("channel", CommandOptionValue::Channel(Id::new(32)))
        .roles([MODERATOR])
        .user(50);
    harness.invoke(all).await.unwrap();
    let mut moved: Vec<u64> = harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::MoveMember {
                user_id,
                channel_id,
                ..
            } if channel_id.get() == 32 => Some(user_id.get()),
            _ => None,
        })
        .collect();
    moved.sort_unstable();
    assert_eq!(moved, [7, 8]);
    let same = Invocation::new("vcmove")
        .option("from", CommandOptionValue::Channel(Id::new(30)))
        .option("channel", CommandOptionValue::Channel(Id::new(30)))
        .roles([MODERATOR])
        .user(50);
    assert!(
        harness.invoke(same).await.unwrap()[0]
            .content
            .contains("different")
    );
}

#[tokio::test]
async fn a_member_who_hops_between_voice_channels_too_often_is_disconnected() {
    let harness = harness();
    harness.set_config(serde_json::json!({ "voice_hop_limit": 3, "voice_hop_seconds": 60 }));
    let hop = |to: u64| voice_move(7, None, Some(to));
    for to in [30, 31, 30] {
        harness.emit(hop(to)).await.unwrap();
    }
    assert_eq!(disconnected(&harness, 7), 0, "three is the limit");
    harness.emit(hop(31)).await.unwrap();
    assert_eq!(disconnected(&harness, 7), 1);
    // The count starts again, and a slow member is never caught.
    harness.emit(hop(30)).await.unwrap();
    assert_eq!(disconnected(&harness, 7), 1);
    for to in [31, 30, 31] {
        harness.advance(std::time::Duration::from_secs(61));
        harness.emit(hop(to)).await.unwrap();
    }
    assert_eq!(disconnected(&harness, 7), 1);
}

#[tokio::test]
async fn without_a_limit_voice_hops_are_not_counted() {
    let harness = harness();
    for to in [30, 31, 30, 31, 30, 31, 30] {
        harness.emit(voice_move(7, None, Some(to))).await.unwrap();
    }
    assert_eq!(disconnected(&harness, 7), 0);
}

#[tokio::test]
async fn a_moderator_changes_and_resets_a_nickname_and_an_id_tells_its_age() {
    let harness = harness();
    harness.api().set_member(Id::new(1), member_of(7));
    let set = |nick: Option<&str>, roles: &[u64]| {
        let invocation = Invocation::new("nickname")
            .option("user", user_option(7))
            .roles(roles.iter().copied())
            .user(50);
        match nick {
            Some(nick) => {
                invocation.option("nickname", CommandOptionValue::String(nick.to_owned()))
            }
            None => invocation,
        }
    };
    let nicknames = |h: &Harness| -> Vec<Option<String>> {
        h.actions()
            .iter()
            .filter_map(|a| match a {
                Action::SetNickname { nickname, .. } => Some(nickname.clone()),
                _ => None,
            })
            .collect()
    };
    let _ = harness.invoke(set(Some("Nope"), &[])).await;
    assert!(nicknames(&harness).is_empty());
    harness
        .invoke(set(Some("Fresh"), &[MODERATOR]))
        .await
        .unwrap();
    harness.invoke(set(None, &[MODERATOR])).await.unwrap();
    assert_eq!(nicknames(&harness), [Some("Fresh".to_owned()), None]);
    let id = Invocation::new("snowflake")
        .option(
            "id",
            CommandOptionValue::String("175928847299117063".to_owned()),
        )
        .user(50);
    let reply = harness.invoke(id).await.unwrap();
    assert!(reply[0].content.contains("<t:1462015105:F>"), "{reply:?}");
    let bad = Invocation::new("snowflake")
        .option("id", CommandOptionValue::String("abc".to_owned()))
        .user(50);
    assert!(
        harness.invoke(bad).await.unwrap()[0]
            .content
            .contains("not an id")
    );
}
