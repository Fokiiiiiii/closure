use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::{Event, GuildId, MemberInfo, UserId, UserInfo};
use closure_sdk::testing::{Harness, Invocation};

use crate::{MAX_ATTEMPTS, Pass, Stop, builder, clean_page, dehoisted, shown_name};

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

#[test]
fn the_name_is_the_nickname_then_the_global_name_then_the_username() {
    assert_eq!(
        shown_name(Some("Nick"), Some("Global"), Some("user")),
        Some("Nick")
    );
    assert_eq!(
        shown_name(None, Some("Global"), Some("user")),
        Some("Global")
    );
    assert_eq!(shown_name(None, None, Some("user")), Some("user"));
    assert_eq!(shown_name(None, None, None), None);
}

#[tokio::test]
async fn a_hoisting_nickname_is_cleaned_when_it_is_set_or_changed() {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness
        .emit(Event::MemberUpdated(member("Bob", Some("-- Rob"))))
        .await
        .unwrap();
    harness
        .emit(Event::MemberUpdated(member("Bob", Some("Rob"))))
        .await
        .unwrap();
    assert_eq!(nicknames(&harness), vec![Some("Rob".to_owned())]);
}

#[tokio::test]
async fn a_hoisting_username_without_a_nickname_is_left_alone() {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness
        .emit(Event::MemberJoined(member("!Bob", None)))
        .await
        .unwrap();
    assert!(nicknames(&harness).is_empty());
}

#[tokio::test]
async fn the_sweep_starts_from_the_saved_cursor_and_forgets_it_when_done() {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.store().set("sweep-cursor", &500_u64).await.unwrap();
    harness
        .invoke(Invocation::new("dehoist").administrator())
        .await
        .unwrap();
    assert!(harness.actions().iter().any(|action| matches!(
        action,
        Action::ListMembers { after: Some(id), .. } if id.get() == 500
    )));
    assert_eq!(
        harness.store().get::<u64>("sweep-cursor").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn failed_changes_count_against_the_budget() {
    let total = u64::try_from(MAX_ATTEMPTS + 10).unwrap();
    let shown: Vec<(UserId, Option<String>)> = (1..=total)
        .map(|id| (UserId::new(id), Some("!x".to_owned())))
        .collect();
    let mut pass = Pass::default();
    clean_page(&shown, "member", &mut pass, |_, _| async {
        Err::<(), _>(DiscordError::NotFound)
    })
    .await;
    assert_eq!(pass.attempts, MAX_ATTEMPTS);
    assert_eq!(pass.stop, Some(Stop::Budget));
    assert_eq!(
        pass.last,
        Some(UserId::new(u64::try_from(MAX_ATTEMPTS).unwrap()))
    );
}

#[tokio::test]
async fn a_run_of_refusals_stops_the_sweep() {
    let shown: Vec<(UserId, Option<String>)> = (1..=10)
        .map(|id| (UserId::new(id), Some("!x".to_owned())))
        .collect();
    let mut pass = Pass::default();
    clean_page(&shown, "member", &mut pass, |_, _| async {
        Err::<(), _>(DiscordError::Forbidden("no".into()))
    })
    .await;
    assert_eq!(pass.stop, Some(Stop::Refused));
    assert_eq!(pass.attempts, 5);
    assert_eq!(pass.last, Some(UserId::new(4)));
}
