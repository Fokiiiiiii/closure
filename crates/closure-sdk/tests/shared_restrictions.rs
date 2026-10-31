//! Two modules of one bot share the roles a restriction took off a member: a timeout by one of them
//! takes the roles off, and a lift by the other gives them back.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::time::Duration;

use closure_core::action::Action;
use closure_sdk::punish::{self, Method, Moderator, Punisher, RoleSelection, Settings, Strip};
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Capability, Command, CommandCx, ModuleBuilder, UserError};
use serde_json::json;
use twilight_model::guild::{Member, Role};
use twilight_model::id::Id;

const GUILD: u64 = 1;
const BOT: u64 = 9_999;
const BOT_ROLE: u64 = 900;
const TIMEOUT_ROLE: u64 = 700;
const STAFF: u64 = 800;
const HELPER: u64 = 801;
const TARGET: u64 = 500;
const MODERATOR: u64 = 100;

fn role(id: u64, name: &str, position: i64) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": name, "color": 0, "hoist": false, "managed": false,
        "mentionable": false, "permissions": "0", "position": position, "flags": 0,
        "icon": null, "unicode_emoji": null, "tags": null,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    }))
    .unwrap()
}

fn member(id: u64, roles: &[u64]) -> Member {
    serde_json::from_value(json!({
        "deaf": false, "mute": false, "flags": 0,
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "user": { "id": id.to_string(), "username": "member", "discriminator": "0",
                  "avatar": null, "bot": false },
    }))
    .unwrap()
}

/// How the two modules give and take the roles: the timeout role, and the roles that a timeout
/// takes off the member and gives back when it lifts.
fn settings() -> Settings {
    Settings {
        dm: false,
        timeout: Method::Role,
        timeout_role: Some(Id::new(TIMEOUT_ROLE)),
        voice: Method::Discord,
        voice_role: None,
        strip: Strip {
            remove: RoleSelection::Some(vec![Id::new(STAFF), Id::new(HELPER)]),
            restore: RoleSelection::Some(vec![Id::new(STAFF), Id::new(HELPER)]),
        },
    }
}

async fn strip(cx: CommandCx, (): ()) -> Result<(), UserError> {
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings())
        .timeout(Id::new(TARGET), Duration::from_secs(3600), None)
        .await?;
    Ok(())
}

async fn lift(cx: CommandCx, (): ()) -> Result<(), UserError> {
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings())
        .lift_timeout(Id::new(TARGET), None)
        .await?;
    Ok(())
}

fn alpha() -> ModuleBuilder {
    let builder = ModuleBuilder::new("alpha", "Alpha", "0.1.0")
        .unwrap()
        .capability(Capability::Cases)
        .capability(Capability::ManageRoles)
        .capability(Capability::ManageChannels)
        .capability(Capability::ReadGuild)
        .capability(Capability::SendMessages)
        .capability(Capability::Schedule)
        .capability(Capability::Storage)
        .config(json!({ "type": "object", "properties": {} }), json!({}))
        .command(Command::<()>::new("strip").handler(strip));
    punish::jobs(builder)
}

fn beta() -> ModuleBuilder {
    let builder = ModuleBuilder::new("beta", "Beta", "0.1.0")
        .unwrap()
        .capability(Capability::Cases)
        .capability(Capability::ManageRoles)
        .capability(Capability::ManageChannels)
        .capability(Capability::ReadGuild)
        .capability(Capability::SendMessages)
        .capability(Capability::Schedule)
        .capability(Capability::Storage)
        .config(json!({ "type": "object", "properties": {} }), json!({}))
        .command(Command::<()>::new("lift").handler(lift));
    punish::jobs(builder)
}

/// The roles that were added to the target, by any module of the server.
fn given(harness: &Harness) -> Vec<u64> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::AddRole {
                user_id, role_id, ..
            } if user_id.get() == TARGET => Some(role_id.get()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_timeout_by_one_module_is_lifted_by_another_and_gives_the_roles_back() {
    let first = Harness::new(alpha()).unwrap();
    let second = first.sibling(beta()).unwrap();
    first.api().set_roles(
        Id::new(GUILD),
        vec![
            role(BOT_ROLE, "Bot", 50),
            role(TIMEOUT_ROLE, "Timed out", 1),
            role(STAFF, "Staff", 1),
            role(HELPER, "Helper", 1),
        ],
    );
    first
        .api()
        .set_member(Id::new(GUILD), member(BOT, &[BOT_ROLE]));
    first
        .api()
        .set_member(Id::new(GUILD), member(TARGET, &[STAFF, HELPER]));

    first
        .invoke(Invocation::new("strip").user(MODERATOR))
        .await
        .unwrap();
    assert!(
        !given(&first).contains(&STAFF),
        "nothing is given back by the timeout itself"
    );

    second
        .invoke(Invocation::new("lift").user(MODERATOR))
        .await
        .unwrap();

    let back = given(&first);
    assert!(
        back.contains(&STAFF) && back.contains(&HELPER),
        "the roles taken off by the other module are given back: {back:?}"
    );
}
