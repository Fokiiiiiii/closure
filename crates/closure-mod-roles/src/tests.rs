use closure_core::action::{Action, Emoji};
use closure_core::event::{ChannelId, Event, GuildId, MessageId, ReactionInfo, RoleId, UserId};
use closure_sdk::testing::{Click, Harness, Invocation};
use closure_sdk::{CommandOptionValue, Level};
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::builder;

const MODERATOR: u64 = 200;
const SELF_ROLE: u64 = 100;
const HIGH_ROLE: u64 = 300;
const DENIED: u64 = 400;

fn role(id: u64, position: i64) -> Role {
    role_with(id, position, 0)
}

fn role_with(id: u64, position: i64, permissions: u64) -> Role {
    serde_json::from_value(serde_json::json!({
        "id": id.to_string(), "name": format!("role-{id}"), "color": 0, "colors": {
            "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "hoist": false, "managed": false, "mentionable": false, "permissions": permissions.to_string(),
        "position": position, "flags": 0,
    }))
    .unwrap()
}

fn member(user: u64) -> twilight_model::guild::Member {
    serde_json::from_value(serde_json::json!({
        "user": {"id": user.to_string(), "username": "member", "discriminator": "0",
                 "avatar": null, "bot": false},
        "roles": [], "joined_at": "2020-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
    }))
    .unwrap()
}

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR), Level::MODERATOR);
    });
    harness.api().set_roles(
        Id::new(1),
        vec![role(SELF_ROLE, 1), role(MODERATOR, 5), role(HIGH_ROLE, 9)],
    );
    harness.set_config(serde_json::json!({
        "self_roles": [SELF_ROLE.to_string()],
        "reaction_roles": [{"channel": "5", "message": "900", "emoji": "✅", "role": SELF_ROLE.to_string()}]
    }));
    harness
}

fn role_option(id: u64) -> CommandOptionValue {
    CommandOptionValue::Role(Id::new(id))
}

fn user_option(id: u64) -> CommandOptionValue {
    CommandOptionValue::User(Id::new(id))
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

#[tokio::test]
async fn a_member_takes_and_leaves_a_listed_role() {
    let harness = harness();
    let join = Invocation::new("role")
        .subcommand("join")
        .option("role", role_option(SELF_ROLE))
        .user(7);
    let reply = harness.invoke(join).await.unwrap();
    assert!(gave(&harness, 7, SELF_ROLE));
    assert!(reply[0].content.contains(&format!("<@&{SELF_ROLE}>")));
    let leave = Invocation::new("role")
        .subcommand("leave")
        .option("role", role_option(SELF_ROLE))
        .user(7);
    harness.invoke(leave).await.unwrap();
    assert!(took(&harness, 7, SELF_ROLE));
}

#[tokio::test]
async fn a_role_that_is_not_listed_is_refused() {
    let harness = harness();
    let join = Invocation::new("role")
        .subcommand("join")
        .option("role", role_option(HIGH_ROLE))
        .user(7);
    let reply = harness.invoke(join).await.unwrap();
    assert!(!gave(&harness, 7, HIGH_ROLE));
    assert!(reply[0].content.contains("not one members may take"));
}

#[tokio::test]
async fn a_moderator_hands_out_only_roles_below_their_own() {
    let harness = harness();
    harness.api().set_member(Id::new(1), member(9));
    let give = |role: u64| {
        Invocation::new("role")
            .subcommand("give")
            .option("user", user_option(9))
            .option("role", role_option(role))
            .roles([MODERATOR])
            .user(50)
    };
    harness.invoke(give(SELF_ROLE)).await.unwrap();
    assert!(gave(&harness, 9, SELF_ROLE));
    let refused = harness.invoke(give(HIGH_ROLE)).await.unwrap();
    assert!(!gave(&harness, 9, HIGH_ROLE));
    assert!(refused[0].content.contains("below your highest role"));
    let take = Invocation::new("role")
        .subcommand("take")
        .option("user", user_option(9))
        .option("role", role_option(SELF_ROLE))
        .roles([MODERATOR])
        .user(50);
    harness.invoke(take).await.unwrap();
    assert!(took(&harness, 9, SELF_ROLE));
}

#[tokio::test]
async fn a_role_with_powers_the_moderator_lacks_is_not_handed_out_whatever_its_position() {
    const ADMIN_ROLE: u64 = 150;
    let harness = harness();
    harness.api().set_roles(
        Id::new(1),
        vec![
            role(SELF_ROLE, 1),
            role_with(ADMIN_ROLE, 2, 0x8),
            role_with(MODERATOR, 5, 0x10_0000),
        ],
    );
    harness.api().set_member(Id::new(1), member(9));
    let give = |role: u64| {
        Invocation::new("role")
            .subcommand("give")
            .option("user", user_option(9))
            .option("role", role_option(role))
            .roles([MODERATOR])
            .user(50)
    };
    let refused = harness.invoke(give(ADMIN_ROLE)).await.unwrap();
    assert!(!gave(&harness, 9, ADMIN_ROLE));
    assert!(
        refused[0].content.contains("permissions you do not have"),
        "{refused:?}"
    );
    harness.invoke(give(SELF_ROLE)).await.unwrap();
    assert!(
        gave(&harness, 9, SELF_ROLE),
        "a role with no powers is still handed out"
    );
}

fn reaction(message: u64, emoji: &str, user: u64) -> ReactionInfo {
    ReactionInfo {
        guild_id: GuildId::new(1),
        channel_id: ChannelId::new(5),
        message_id: MessageId::new(message),
        user_id: UserId::new(user),
        emoji: Emoji::Unicode {
            value: emoji.to_owned(),
        },
    }
}

#[tokio::test]
async fn a_reaction_on_a_role_message_gives_and_takes_back_the_role() {
    let harness = self::harness();
    harness.api().set_member(Id::new(1), member(7));
    harness
        .emit(Event::ReactionAdded(reaction(900, "✅", 7)))
        .await
        .unwrap();
    assert!(gave(&harness, 7, SELF_ROLE));
    harness
        .emit(Event::ReactionRemoved(reaction(900, "✅", 7)))
        .await
        .unwrap();
    assert!(took(&harness, 7, SELF_ROLE));
    let other = self::harness();
    other
        .emit(Event::ReactionAdded(reaction(901, "✅", 7)))
        .await
        .unwrap();
    other
        .emit(Event::ReactionAdded(reaction(900, "❌", 7)))
        .await
        .unwrap();
    assert!(other.actions().is_empty());
}

#[tokio::test]
async fn a_member_who_may_not_pick_roles_gets_none_from_a_button_or_a_reaction() {
    let harness = self::harness();
    // `roles.self` is denied to the member's role: neither way in gives them a role.
    harness.permissions(|permissions| {
        permissions
            .keys
            .entry(closure_core::permission::PermissionKey::new("roles.self").unwrap())
            .or_default()
            .deny_roles
            .push(Id::new(DENIED));
    });
    harness.api().set_member(Id::new(1), {
        let mut denied = member(7);
        denied.roles = vec![Id::new(DENIED)];
        denied
    });
    harness
        .emit(Event::ReactionAdded(reaction(900, "✅", 7)))
        .await
        .unwrap();
    assert!(!gave(&harness, 7, SELF_ROLE));
    let click = Click::new(format!("roles:toggle:*:{SELF_ROLE}"))
        .roles([DENIED])
        .user(7);
    let _ = harness.click(click).await;
    assert!(!gave(&harness, 7, SELF_ROLE));
}

#[tokio::test]
async fn a_zero_id_in_the_settings_does_not_stop_the_list() {
    let harness = harness();
    harness.set_config(serde_json::json!({ "self_roles": ["0", SELF_ROLE.to_string()] }));
    let listed = harness
        .invoke(Invocation::new("role").subcommand("list").user(7))
        .await
        .unwrap();
    assert!(
        listed[0].content.contains(&format!("<@&{SELF_ROLE}>")),
        "{listed:?}"
    );
}

#[tokio::test]
async fn a_button_toggles_a_listed_role() {
    let harness = harness();
    let custom_id = format!("roles:toggle:*:{SELF_ROLE}");
    harness
        .click(Click::new(custom_id.clone()).user(7))
        .await
        .unwrap();
    assert!(gave(&harness, 7, SELF_ROLE));
    harness
        .click(Click::new(custom_id).user(7).roles([SELF_ROLE]))
        .await
        .unwrap();
    assert!(took(&harness, 7, SELF_ROLE));
    let unlisted = format!("roles:toggle:*:{HIGH_ROLE}");
    harness.click(Click::new(unlisted).user(7)).await.unwrap();
    assert!(!gave(&harness, 7, HIGH_ROLE));
    let _ = RoleId::new(1);
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
async fn a_listed_role_can_be_pinged_for_a_minute_and_then_is_closed_again() {
    let harness = self::harness();
    harness.set_config(serde_json::json!({"pingable_roles": [SELF_ROLE.to_string()]}));
    let ping = |role: u64| {
        Invocation::new("pingrole")
            .option("role", role_option(role))
            .roles([MODERATOR])
            .user(50)
    };
    harness.invoke(ping(SELF_ROLE)).await.unwrap();
    let editing = |mentionable: bool| {
        harness.actions().iter().any(|a| {
            matches!(a, Action::EditRole { role_id, patch, .. }
                if role_id.get() == SELF_ROLE && patch.mentionable == Some(mentionable))
        })
    };
    assert!(editing(true) && !editing(false));
    assert_eq!(harness.jobs(), 1);
    harness.advance(std::time::Duration::from_secs(61));
    harness.run_due_jobs().await;
    assert!(editing(false));
    let refused = harness.invoke(ping(HIGH_ROLE)).await.unwrap();
    assert!(refused[0].content.contains("can be pinged this way"));
}

#[tokio::test]
async fn the_role_of_a_punishment_is_not_handed_out_or_taken_by_hand_or_picked() {
    let harness = harness();
    harness.api().set_member(Id::new(1), member(9));
    harness.set_config(serde_json::json!({
        "self_roles": [SELF_ROLE.to_string()],
        "reaction_roles": []
    }));
    harness
        .restrictions()
        .set(&format!("restriction-role:{SELF_ROLE}"), &SELF_ROLE)
        .await
        .unwrap();
    let hand = |sub: &str| {
        Invocation::new("role")
            .subcommand(sub)
            .option("user", user_option(9))
            .option("role", role_option(SELF_ROLE))
            .roles([MODERATOR])
            .user(50)
    };
    let given = harness.invoke(hand("give")).await.unwrap();
    assert!(
        given[0].content.contains("belongs to a punishment"),
        "{given:?}"
    );
    harness.invoke(hand("take")).await.unwrap();
    let own = Invocation::new("role")
        .subcommand("join")
        .option("role", role_option(SELF_ROLE))
        .user(51);
    harness.invoke(own).await.unwrap();
    assert!(!gave(&harness, 9, SELF_ROLE) && !took(&harness, 9, SELF_ROLE));
    assert!(!gave(&harness, 51, SELF_ROLE));
}
