use closure_core::action::Action;
use closure_core::event::{Event, GuildId, MemberInfo, UserId, UserInfo};
use closure_sdk::testing::Harness;

use crate::{builder, dehoisted};

fn member(name: &str, nick: Option<&str>) -> MemberInfo {
    MemberInfo {
        guild_id: GuildId::new(1),
        user: UserInfo {
            id: UserId::new(7),
            name: name.into(),
            bot: false,
        },
        roles: Vec::new(),
        nick: nick.map(Into::into),
        joined_at: None,
        timed_out_until: None,
    }
}

fn nicknames(harness: &Harness) -> Vec<Option<String>> {
    harness
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SetNickname { nickname, .. } => Some(nickname.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn only_a_name_that_starts_with_a_symbol_is_changed() {
    assert_eq!(dehoisted("!!Bob", "member").as_deref(), Some("Bob"));
    assert_eq!(dehoisted("  ( Bob", "member").as_deref(), Some("Bob"));
    assert_eq!(dehoisted("Bob!", "member"), None);
    assert_eq!(dehoisted("~Bob", "member"), None);
    assert_eq!(dehoisted("!!!", "member").as_deref(), Some("member"));
    assert_eq!(
        dehoisted(&format!("!{}", "a".repeat(40)), "m")
            .unwrap()
            .len(),
        32
    );
}

#[tokio::test]
async fn a_member_who_joins_or_renames_to_a_hoisting_name_is_cleaned() {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness
        .emit(Event::MemberJoined(member("!Bob", None)))
        .await
        .unwrap();
    harness
        .emit(Event::MemberUpdated(member("Bob", Some("-- Rob"))))
        .await
        .unwrap();
    harness
        .emit(Event::MemberUpdated(member("Bob", Some("Rob"))))
        .await
        .unwrap();
    assert_eq!(
        nicknames(&harness),
        vec![Some("Bob".to_owned()), Some("Rob".to_owned())]
    );
}
