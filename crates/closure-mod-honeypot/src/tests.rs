use closure_core::action::Action;
use closure_sdk::testing::{Harness, Invocation, Message};
use closure_sdk::{CommandOptionValue, Level};
use serde_json::{Value, json};
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::builder;

const GUILD: u64 = 1;
const TRAP: u64 = 40;
const OTHER: u64 = 41;
const ADMIN_ROLE: u64 = 300;
const MOD_ROLE: u64 = 301;
const PLAIN_ROLE: u64 = 302;
const SPAMMER: u64 = 7;

fn role(id: u64, permissions: u64) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": format!("role-{id}"), "color": 0, "colors": {
            "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "hoist": false, "managed": false, "mentionable": false,
        "permissions": permissions.to_string(), "position": 1, "flags": 0,
    }))
    .unwrap()
}

fn harness(config: &Value) -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(ADMIN_ROLE), Level::ADMIN);
    });
    harness.api().set_guild_name(Id::new(GUILD), "server");
    // 4: BAN_MEMBERS.
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(MOD_ROLE, 4), role(PLAIN_ROLE, 0)]);
    for channel in [TRAP, OTHER] {
        harness
            .api()
            .add_channel(Id::new(channel), Some(Id::new(GUILD)));
    }
    harness.set_config(config.clone());
    harness
}

fn config(changes: &Value) -> Value {
    let mut config = json!({
        "action": "ban", "delete_hours": 24, "timeout": "1d",
        "exempt_users": [], "exempt_roles": [],
    });
    for (key, value) in changes.as_object().unwrap() {
        config[key] = value.clone();
    }
    config
}

async fn trap(harness: &Harness) {
    harness
        .invoke(
            Invocation::new("honeypot")
                .subcommand("add")
                .option("channel", CommandOptionValue::Channel(Id::new(TRAP)))
                .user(1)
                .roles([ADMIN_ROLE]),
        )
        .await
        .unwrap();
}

fn count(harness: &Harness, pick: impl Fn(&Action) -> bool) -> usize {
    harness
        .actions()
        .iter()
        .filter(|action| pick(action))
        .count()
}

fn bans(harness: &Harness) -> usize {
    count(harness, |a| matches!(a, Action::Ban { .. }))
}

#[tokio::test]
async fn whoever_posts_in_a_honeypot_is_banned_and_the_message_deleted() {
    let harness = harness(&config(&json!({})));
    trap(&harness).await;
    harness
        .message(
            Message::new(SPAMMER, TRAP)
                .roles([PLAIN_ROLE])
                .content("free nitro"),
        )
        .await
        .unwrap();
    assert_eq!(bans(&harness), 1);
    assert_eq!(
        count(&harness, |a| matches!(a, Action::DeleteMessage { .. })),
        1
    );
    assert!(harness.actions().iter().any(|a| matches!(
        a,
        Action::Ban { delete_message_seconds, .. } if *delete_message_seconds == 86_400
    )));
    let case = harness
        .cases()
        .get(1)
        .await
        .unwrap()
        .expect("a case is made");
    assert_eq!(case.kind.as_str(), "ban");
}

#[tokio::test]
async fn the_action_follows_the_setting() {
    let harness = harness(&config(&json!({ "action": "kick" })));
    trap(&harness).await;
    harness
        .message(Message::new(SPAMMER, TRAP).content("hi"))
        .await
        .unwrap();
    assert_eq!(bans(&harness), 0);
    assert_eq!(count(&harness, |a| matches!(a, Action::Kick { .. })), 1);
}

#[tokio::test]
async fn other_channels_bots_staff_and_the_listed_are_left_alone() {
    let harness = harness(&config(&json!({ "exempt_users": ["8"] })));
    trap(&harness).await;
    harness
        .message(Message::new(SPAMMER, OTHER).content("hi"))
        .await
        .unwrap();
    harness
        .message(Message::new(9, TRAP).bot().content("beep"))
        .await
        .unwrap();
    harness
        .message(Message::new(10, TRAP).roles([MOD_ROLE]).content("oops"))
        .await
        .unwrap();
    harness
        .message(Message::new(8, TRAP).content("listed"))
        .await
        .unwrap();
    harness
        .message(Message::new(999_999_999, TRAP).content("owner"))
        .await
        .unwrap();
    assert_eq!(bans(&harness), 0);
    assert_eq!(
        count(&harness, |a| matches!(a, Action::DeleteMessage { .. })),
        0
    );
}

#[tokio::test]
async fn create_makes_a_channel_with_a_warning_and_sets_it_as_a_honeypot() {
    let harness = harness(&config(&json!({})));
    let replies = harness
        .invoke(
            Invocation::new("honeypot")
                .subcommand("create")
                .user(1)
                .roles([ADMIN_ROLE]),
        )
        .await
        .unwrap();
    assert!(replies.last().unwrap().content.contains("honeypot"));
    assert_eq!(
        count(&harness, |a| matches!(a, Action::CreateChannel { .. })),
        1
    );
    assert_eq!(
        count(&harness, |a| matches!(a, Action::SendMessage { .. })),
        1
    );
    let listed = harness
        .invoke(
            Invocation::new("honeypot")
                .subcommand("list")
                .user(1)
                .roles([ADMIN_ROLE]),
        )
        .await
        .unwrap();
    assert!(listed.last().unwrap().content.contains("<#"));
}
