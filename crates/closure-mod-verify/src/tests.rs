use std::time::Duration;

use closure_core::action::Action;
use closure_core::event::{Event, MemberInfo, UserInfo};
use closure_sdk::testing::{Click, Harness, Invocation, ReplyKind, Submit};
use closure_sdk::{Level, ModuleBuilder};
use serde_json::json;
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::builder;

const VERIFIED: u64 = 100;
const UNVERIFIED: u64 = 101;
const ADMIN_ROLE: u64 = 300;
const MEMBER: u64 = 7;
const PRESS: &str = "verify:press:*:";

fn role(id: u64, permissions: u64) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": format!("role-{id}"), "color": 0, "colors": {
            "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "hoist": false, "managed": false, "mentionable": false,
        "permissions": permissions.to_string(), "position": 1, "flags": 0,
    }))
    .unwrap()
}

fn member(id: u64, roles: &[u64]) -> twilight_model::guild::Member {
    serde_json::from_value(json!({
        "user": {"id": id.to_string(), "username": "member", "discriminator": "0",
                 "avatar": null, "bot": false},
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "joined_at": "2020-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
    }))
    .unwrap()
}

fn harness(config: serde_json::Value) -> Harness {
    let builder: ModuleBuilder = builder().unwrap();
    let harness = Harness::new(builder).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(ADMIN_ROLE), Level::ADMIN);
    });
    harness.api().set_roles(
        Id::new(1),
        vec![role(VERIFIED, 0), role(UNVERIFIED, 0), role(ADMIN_ROLE, 0)],
    );
    harness.set_config(config);
    harness
}

fn gate() -> serde_json::Value {
    json!({
        "role": VERIFIED.to_string(),
        "unverified_role": UNVERIFIED.to_string(),
        "mode": "button",
        "kick_after": null,
    })
}

fn gave(harness: &Harness, user: u64, role: u64) -> bool {
    harness.actions().iter().any(|a| {
        matches!(a, Action::AddRole { user_id, role_id, .. }
            if user_id.get() == user && role_id.get() == role)
    })
}

fn took(harness: &Harness, user: u64, role: u64) -> bool {
    harness.actions().iter().any(|a| {
        matches!(a, Action::RemoveRole { user_id, role_id, .. }
            if user_id.get() == user && role_id.get() == role)
    })
}

fn kicked(harness: &Harness, user: u64) -> bool {
    harness
        .actions()
        .iter()
        .any(|a| matches!(a, Action::Kick { user_id, .. } if user_id.get() == user))
}

fn joined(user: u64, bot: bool) -> Event {
    Event::MemberJoined(MemberInfo {
        guild_id: Id::new(1),
        user: UserInfo {
            id: Id::new(user),
            name: "newcomer".into(),
            bot,
        },
        roles: Vec::new(),
        nick: None,
        joined_at: None,
        timed_out_until: None,
    })
}

#[tokio::test]
async fn the_button_gives_the_verified_role_and_takes_the_unverified_one() {
    let harness = harness(gate());
    let replies = harness.click(Click::new(PRESS).user(MEMBER)).await.unwrap();
    assert!(gave(&harness, MEMBER, VERIFIED), "{replies:?}");
    assert!(took(&harness, MEMBER, UNVERIFIED));
}

#[tokio::test]
async fn a_member_who_is_verified_already_is_told_and_nothing_is_given() {
    let harness = harness(gate());
    let replies = harness
        .click(Click::new(PRESS).user(MEMBER).roles([VERIFIED]))
        .await
        .unwrap();
    assert!(!gave(&harness, MEMBER, VERIFIED));
    assert!(
        replies.last().unwrap().content.contains("already"),
        "{replies:?}"
    );
}

#[tokio::test]
async fn a_role_with_moderator_powers_is_never_given_by_the_button() {
    let harness = harness(gate());
    // The verified role was set to one that can ban: the button refuses.
    harness.api().set_roles(
        Id::new(1),
        vec![role(VERIFIED, 1 << 2), role(UNVERIFIED, 0)],
    );
    let replies = harness.click(Click::new(PRESS).user(MEMBER)).await.unwrap();
    assert!(!gave(&harness, MEMBER, VERIFIED), "{replies:?}");
    assert!(
        replies.last().unwrap().content.contains("moderator powers"),
        "{replies:?}"
    );
    // So does a server that set nothing.
    harness.set_config(json!({ "mode": "button" }));
    let unset = harness.click(Click::new(PRESS).user(MEMBER)).await.unwrap();
    assert!(
        unset.last().unwrap().content.contains("not set up"),
        "{unset:?}"
    );
}

#[tokio::test]
async fn the_question_mode_asks_a_sum_once_and_gives_the_role_for_the_right_answer() {
    let mut config = gate();
    config["mode"] = json!("question");
    let harness = harness(config);
    let asked = harness.click(Click::new(PRESS).user(MEMBER)).await.unwrap();
    assert_eq!(asked.last().unwrap().kind, ReplyKind::Modal, "{asked:?}");
    assert!(
        !gave(&harness, MEMBER, VERIFIED),
        "nothing before the answer"
    );

    let sum: u64 = harness
        .store()
        .get("sum:7")
        .await
        .unwrap()
        .expect("the sum was kept");
    let wrong = harness
        .submit(
            Submit::new("verify:answer:7:")
                .user(MEMBER)
                .field("verify:answer", &(sum + 1).to_string()),
        )
        .await
        .unwrap();
    assert!(
        wrong.last().unwrap().content.contains("not the answer"),
        "{wrong:?}"
    );
    assert!(!gave(&harness, MEMBER, VERIFIED));
    // A sum can be tried once: the right answer to the old one is no use.
    let late = harness
        .submit(
            Submit::new("verify:answer:7:")
                .user(MEMBER)
                .field("verify:answer", &sum.to_string()),
        )
        .await
        .unwrap();
    assert!(!gave(&harness, MEMBER, VERIFIED), "{late:?}");

    harness.click(Click::new(PRESS).user(MEMBER)).await.unwrap();
    let sum: u64 = harness.store().get("sum:7").await.unwrap().unwrap();
    harness
        .submit(
            Submit::new("verify:answer:7:")
                .user(MEMBER)
                .field("verify:answer", &sum.to_string()),
        )
        .await
        .unwrap();
    assert!(gave(&harness, MEMBER, VERIFIED));
}

#[tokio::test]
async fn a_form_cannot_be_sent_by_somebody_else() {
    let mut config = gate();
    config["mode"] = json!("question");
    let harness = harness(config);
    harness.click(Click::new(PRESS).user(MEMBER)).await.unwrap();
    let sum: u64 = harness.store().get("sum:7").await.unwrap().unwrap();
    harness
        .submit(
            Submit::new("verify:answer:7:")
                .user(8)
                .field("verify:answer", &sum.to_string()),
        )
        .await
        .unwrap();
    assert!(!gave(&harness, 8, VERIFIED));
    assert!(!gave(&harness, MEMBER, VERIFIED));
}

#[tokio::test]
async fn a_new_member_gets_the_unverified_role_and_is_kicked_when_the_time_is_up() {
    let mut config = gate();
    config["kick_after"] = json!("1h");
    let harness = harness(config);
    harness.emit(joined(MEMBER, false)).await.unwrap();
    assert!(gave(&harness, MEMBER, UNVERIFIED));
    assert_eq!(harness.jobs(), 1);

    harness
        .api()
        .set_member(Id::new(1), member(MEMBER, &[UNVERIFIED]));
    harness.advance(Duration::from_secs(3_700));
    let ended = harness.run_due_jobs().await;
    assert!(ended.iter().all(Result::is_ok), "{ended:?}");
    assert!(kicked(&harness, MEMBER));
    let case = harness
        .cases()
        .get(1)
        .await
        .unwrap()
        .expect("the kick is a case");
    assert_eq!(case.kind.as_str(), "kick");
}

#[tokio::test]
async fn a_member_who_verified_in_time_is_not_kicked_and_a_bot_is_left_alone() {
    let mut config = gate();
    config["kick_after"] = json!("1h");
    let harness = harness(config);
    harness.emit(joined(MEMBER, false)).await.unwrap();
    harness.emit(joined(8, true)).await.unwrap();
    assert!(!gave(&harness, 8, UNVERIFIED), "a bot is not gated");
    assert_eq!(harness.jobs(), 1, "only the member has an expiry");

    harness.click(Click::new(PRESS).user(MEMBER)).await.unwrap();
    assert_eq!(harness.jobs(), 0, "verifying cancels the expiry");
    harness.advance(Duration::from_secs(3_700));
    harness.run_due_jobs().await;
    assert!(!kicked(&harness, MEMBER));
}

#[tokio::test]
async fn only_an_administrator_posts_the_panel_and_it_is_public_with_one_button() {
    let harness = harness(gate());
    let denied = harness
        .invoke(Invocation::new("verify").user(MEMBER))
        .await
        .unwrap();
    assert!(
        denied.last().unwrap().content.contains("level"),
        "{denied:?}"
    );

    let posted = harness
        .invoke(Invocation::new("verify").user(MEMBER).roles([ADMIN_ROLE]))
        .await
        .unwrap();
    let panel = posted.last().unwrap();
    assert!(!panel.ephemeral);
    assert_eq!(panel.custom_ids().len(), 1);
}
