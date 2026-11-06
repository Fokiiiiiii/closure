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
    harness
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
    let kick = Invocation::new("vckick")
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
