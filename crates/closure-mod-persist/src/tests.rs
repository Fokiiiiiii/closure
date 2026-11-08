use closure_core::action::Action;
use closure_core::event::{Event, GuildId, MemberInfo, RoleId, UserId, UserInfo};
use closure_sdk::testing::Harness;

use crate::builder;

fn member(user: u64, roles: &[u64], nick: Option<&str>) -> MemberInfo {
    MemberInfo {
        guild_id: GuildId::new(1),
        user: UserInfo {
            id: UserId::new(user),
            name: format!("user-{user}"),
            bot: false,
        },
        roles: roles.iter().map(|r| RoleId::new(*r)).collect(),
        nick: nick.map(str::to_owned),
        joined_at: None,
        timed_out_until: None,
    }
}

fn harness(nickname: bool) -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.set_config(serde_json::json!({"roles": ["10", "11"], "nickname": nickname}));
    harness
}

#[tokio::test]
async fn a_member_gets_back_the_kept_roles_when_they_rejoin() {
    let harness = harness(false);
    harness
        .emit(Event::MemberUpdated(member(7, &[10, 99], None)))
        .await
        .unwrap();
    harness
        .emit(Event::MemberJoined(member(7, &[], None)))
        .await
        .unwrap();
    let given: Vec<u64> = harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::AddRole {
                user_id, role_id, ..
            } if user_id.get() == 7 => Some(role_id.get()),
            _ => None,
        })
        .collect();
    assert_eq!(given, [10]);
    harness
        .emit(Event::MemberJoined(member(7, &[], None)))
        .await
        .unwrap();
    assert_eq!(harness.actions().len(), 1);
}

#[tokio::test]
async fn the_nickname_comes_back_only_when_the_server_asked() {
    for (asked, expected) in [(true, true), (false, false)] {
        let harness = harness(asked);
        harness
            .emit(Event::MemberUpdated(member(7, &[10], Some("Nick"))))
            .await
            .unwrap();
        harness
            .emit(Event::MemberJoined(member(7, &[], None)))
            .await
            .unwrap();
        let named = harness
            .actions()
            .iter()
            .any(|a| matches!(a, Action::SetNickname { nickname: Some(n), .. } if n == "Nick"));
        assert_eq!(named, expected);
    }
}

#[tokio::test]
async fn nothing_is_kept_of_a_member_without_kept_roles_and_bots_are_ignored() {
    let harness = harness(false);
    harness
        .emit(Event::MemberUpdated(member(7, &[99], None)))
        .await
        .unwrap();
    harness
        .emit(Event::MemberJoined(member(7, &[], None)))
        .await
        .unwrap();
    assert!(harness.actions().is_empty());
    let mut bot = member(8, &[10], None);
    bot.user.bot = true;
    harness
        .emit(Event::MemberUpdated(bot.clone()))
        .await
        .unwrap();
    harness.emit(Event::MemberJoined(bot)).await.unwrap();
    assert!(harness.actions().is_empty());
}

#[tokio::test]
async fn a_role_not_given_back_is_not_forgotten_when_the_ones_that_were_make_the_member_update() {
    let harness = harness(false);
    harness
        .emit(Event::MemberUpdated(member(7, &[10, 11], None)))
        .await
        .unwrap();
    harness.api().fail_role(
        closure_core::event::RoleId::new(11),
        closure_core::error::DiscordError::Transient("busy".into()),
    );
    let _ = harness
        .emit(Event::MemberJoined(member(7, &[], None)))
        .await;
    harness
        .emit(Event::MemberUpdated(member(7, &[10], None)))
        .await
        .unwrap();
    harness.api().recover_roles();
    harness
        .emit(Event::MemberJoined(member(7, &[10], None)))
        .await
        .unwrap();
    let given: Vec<u64> = harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::AddRole { role_id, .. } => Some(role_id.get()),
            _ => None,
        })
        .collect();
    assert!(
        given.contains(&11),
        "the role that was not given back is still to be given: {given:?}"
    );
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

#[tokio::test]
async fn a_busy_discord_is_asked_again_and_a_refusal_is_not() {
    let busy = harness(false);
    busy.emit(Event::MemberUpdated(member(7, &[10], None)))
        .await
        .unwrap();
    busy.api().fail_nth(
        1,
        closure_core::error::DiscordError::Transient("busy".into()),
    );
    busy.emit(Event::MemberJoined(member(7, &[], None)))
        .await
        .unwrap();
    let given = |h: &Harness| {
        h.actions()
            .iter()
            .filter(|a| matches!(a, Action::AddRole { .. }))
            .count()
    };
    assert_eq!(given(&busy), 1, "the second try got through");

    let refused = harness(false);
    refused
        .emit(Event::MemberUpdated(member(7, &[10], None)))
        .await
        .unwrap();
    refused.api().fail_nth(
        1,
        closure_core::error::DiscordError::Forbidden("above the bot".into()),
    );
    refused
        .emit(Event::MemberJoined(member(7, &[], None)))
        .await
        .unwrap();
    assert_eq!(given(&refused), 0, "a refusal is not tried again");
}
