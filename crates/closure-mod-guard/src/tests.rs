use closure_core::action::Action;
use closure_core::event::{ChannelInfo, Event, UserInfo};
use closure_sdk::testing::Harness;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use twilight_model::guild::Role;
use twilight_model::guild::audit_log::{AuditLogEntry, AuditLogEventType};
use twilight_model::id::Id;

use crate::builder;

const GUILD: u64 = 1;
const LOG: u64 = 77;
const ADMIN_ROLE: u64 = 500;
const PLAIN_ROLE: u64 = 501;
const TRUSTED_ROLE: u64 = 502;
/// The owner the pretend Discord gives every server.
const OWNER: u64 = 999_999_999;
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

fn role(id: u64, permissions: u64) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": format!("role-{id}"), "color": 0, "colors": {
            "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "hoist": false, "managed": false, "mentionable": false,
        "permissions": permissions.to_string(), "position": 1, "flags": 0,
    }))
    .unwrap()
}

fn member(id: u64, roles: &[u64], bot: bool) -> twilight_model::guild::Member {
    serde_json::from_value(json!({
        "user": {"id": id.to_string(), "username": "member", "discriminator": "0",
                 "avatar": null, "bot": bot},
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "joined_at": "2020-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
    }))
    .unwrap()
}

fn harness(config: &Value) -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.set_log_channel(LOG);
    harness.api().set_guild_name(Id::new(GUILD), "server");
    harness.api().set_roles(
        Id::new(GUILD),
        vec![
            role(ADMIN_ROLE, 8),
            role(PLAIN_ROLE, 0),
            role(TRUSTED_ROLE, 0),
        ],
    );
    harness.set_config(config.clone());
    harness
}

fn config(changes: &Value) -> Value {
    let mut config = json!({
        "channel_deletes": 3,
        "role_deletes": 3,
        "bans": 10,
        "window": "30s",
        "response": "strip",
        "alert_channel": null,
        "exempt_users": [],
        "exempt_roles": [],
    });
    for (key, value) in changes.as_object().unwrap() {
        config[key] = value.clone();
    }
    config
}

/// An audit log entry of `kind` on `target`, made by `by` just now.
fn audit(kind: AuditLogEventType, target: u64, by: u64) -> AuditLogEntry {
    let now_ms = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    AuditLogEntry {
        action_type: kind,
        changes: Vec::new(),
        guild_id: None,
        id: Id::new((now_ms - DISCORD_EPOCH_MS) << 22),
        options: None,
        reason: None,
        target_id: Some(Id::new(target)),
        user_id: Some(Id::new(by)),
    }
}

fn channel(id: u64) -> ChannelInfo {
    ChannelInfo {
        id: Id::new(id),
        guild_id: Id::new(GUILD),
        name: Some("gone".into()),
        parent_id: None,
        topic: None,
        nsfw: false,
        overwrites: Vec::new(),
    }
}

/// `by` deletes the channels `ids`, one after the other.
async fn delete_channels(harness: &Harness, by: u64, ids: &[u64]) {
    for id in ids {
        harness
            .api()
            .add_audit_entry(audit(AuditLogEventType::ChannelDelete, *id, by));
        harness
            .emit(Event::ChannelDeleted(channel(*id)))
            .await
            .unwrap();
    }
}

fn took_role(harness: &Harness, user: u64, role: u64) -> bool {
    harness.actions().iter().any(|a| {
        matches!(a, Action::RemoveRole { user_id, role_id, .. }
            if user_id.get() == user && role_id.get() == role)
    })
}

fn took_any(harness: &Harness, user: u64) -> bool {
    harness
        .actions()
        .iter()
        .any(|a| matches!(a, Action::RemoveRole { user_id, .. } if user_id.get() == user))
}

fn alerts(harness: &Harness) -> Vec<String> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::SendMessage {
                channel_id,
                content: Some(text),
                ..
            } if channel_id.get() == LOG => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_account_that_deletes_channels_in_a_burst_loses_its_powers_and_the_staff_are_told() {
    let harness = harness(&config(&json!({})));
    let nuker = 1_001;
    harness.api().set_member(
        Id::new(GUILD),
        member(nuker, &[ADMIN_ROLE, PLAIN_ROLE], false),
    );

    delete_channels(&harness, nuker, &[11, 12]).await;
    assert!(!took_any(&harness, nuker), "two are below the limit");
    assert!(alerts(&harness).is_empty());

    delete_channels(&harness, nuker, &[13]).await;
    assert!(
        took_role(&harness, nuker, ADMIN_ROLE),
        "the powerful role goes"
    );
    assert!(
        !took_role(&harness, nuker, PLAIN_ROLE),
        "a role without powers stays"
    );
    let told = alerts(&harness);
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].contains(&format!("<@{nuker}>")) && told[0].contains('3'),
        "{told:?}"
    );

    // The rest of the burst is not told again.
    delete_channels(&harness, nuker, &[14, 15, 16]).await;
    assert_eq!(alerts(&harness).len(), 1);
}

#[tokio::test]
async fn the_alert_mode_only_tells_the_staff() {
    let harness = harness(&config(&json!({ "response": "alert" })));
    let nuker = 1_002;
    harness
        .api()
        .set_member(Id::new(GUILD), member(nuker, &[ADMIN_ROLE], false));
    delete_channels(&harness, nuker, &[21, 22, 23]).await;
    assert!(!took_any(&harness, nuker));
    assert_eq!(alerts(&harness).len(), 1);
}

#[tokio::test]
async fn a_bot_that_nukes_is_kicked() {
    let harness = harness(&config(&json!({})));
    let rogue = 1_003;
    harness
        .api()
        .set_member(Id::new(GUILD), member(rogue, &[ADMIN_ROLE], true));
    delete_channels(&harness, rogue, &[31, 32, 33]).await;
    assert!(
        harness
            .actions()
            .iter()
            .any(|a| matches!(a, Action::Kick { user_id, .. } if user_id.get() == rogue))
    );
    assert!(!took_any(&harness, rogue), "a bot is kicked, not stripped");
}

#[tokio::test]
async fn bans_and_role_deletions_are_counted_by_their_own_limits() {
    let harness = harness(&config(&json!({ "bans": 2, "role_deletes": 2 })));
    let banner = 1_004;
    harness
        .api()
        .set_member(Id::new(GUILD), member(banner, &[ADMIN_ROLE], false));
    for victim in [41, 42] {
        harness
            .api()
            .add_audit_entry(audit(AuditLogEventType::MemberBanAdd, victim, banner));
        harness
            .emit(Event::BanAdded {
                guild_id: Id::new(GUILD),
                user: UserInfo {
                    id: Id::new(victim),
                    name: "victim".into(),
                    bot: false,
                },
            })
            .await
            .unwrap();
    }
    assert!(took_role(&harness, banner, ADMIN_ROLE));

    let deleter = 1_005;
    harness
        .api()
        .set_member(Id::new(GUILD), member(deleter, &[ADMIN_ROLE], false));
    for role_id in [51, 52] {
        harness
            .api()
            .add_audit_entry(audit(AuditLogEventType::RoleDelete, role_id, deleter));
        harness
            .emit(Event::RoleDeleted {
                guild_id: Id::new(GUILD),
                role_id: Id::new(role_id),
            })
            .await
            .unwrap();
    }
    assert!(took_role(&harness, deleter, ADMIN_ROLE));
}

#[tokio::test]
async fn the_owner_the_trusted_and_the_bot_itself_are_never_acted_on() {
    let harness = harness(&config(&json!({
        "exempt_users": ["1007"],
        "exempt_roles": [TRUSTED_ROLE.to_string()],
    })));
    harness
        .api()
        .set_member(Id::new(GUILD), member(OWNER, &[ADMIN_ROLE], false));
    harness
        .api()
        .set_member(Id::new(GUILD), member(1_007, &[ADMIN_ROLE], false));
    harness.api().set_member(
        Id::new(GUILD),
        member(1_008, &[ADMIN_ROLE, TRUSTED_ROLE], false),
    );
    let bot = harness.bot_id().expect("the pretend bot has an id").get();
    harness
        .api()
        .set_member(Id::new(GUILD), member(bot, &[ADMIN_ROLE], true));
    for (who, first) in [(OWNER, 61), (1_007, 71), (1_008, 81), (bot, 91)] {
        delete_channels(&harness, who, &[first, first + 1, first + 2, first + 3]).await;
        assert!(!took_any(&harness, who), "{who} was acted on");
    }
    assert!(alerts(&harness).is_empty());
}

#[tokio::test]
async fn a_limit_of_zero_a_change_nobody_is_named_for_and_different_accounts_do_not_add_up() {
    let off = harness(&config(&json!({ "channel_deletes": 0 })));
    off.api()
        .set_member(Id::new(GUILD), member(1_010, &[ADMIN_ROLE], false));
    delete_channels(&off, 1_010, &[101, 102, 103, 104]).await;
    assert!(!took_any(&off, 1_010), "not watched");

    // No entry names who deleted them: nobody is blamed.
    let unnamed = harness(&config(&json!({})));
    for id in [111, 112, 113, 114] {
        unnamed
            .emit(Event::ChannelDeleted(channel(id)))
            .await
            .unwrap();
    }
    assert!(alerts(&unnamed).is_empty());

    // Two accounts at two deletions each are not one at four.
    let two = harness(&config(&json!({})));
    two.api()
        .set_member(Id::new(GUILD), member(1_011, &[ADMIN_ROLE], false));
    two.api()
        .set_member(Id::new(GUILD), member(1_012, &[ADMIN_ROLE], false));
    delete_channels(&two, 1_011, &[121, 122]).await;
    delete_channels(&two, 1_012, &[123, 124]).await;
    assert!(alerts(&two).is_empty());
}
