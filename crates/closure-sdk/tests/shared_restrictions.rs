//! Two modules of one bot share the roles a restriction took off a member: a timeout by one of them
//! takes the roles off, and a lift by the other gives them back.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::time::Duration;

use closure_core::action::Action;
use closure_core::error::DiscordError;
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
        dm_messages: closure_sdk::punish::DmMessages::default(),
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

async fn strip_legacy(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let mut settings = settings();
    settings.strip.remove = RoleSelection::Some(vec![Id::new(701)]);
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings)
        .timeout(Id::new(TARGET), Duration::from_secs(3600), None)
        .await?;
    Ok(())
}

async fn migrate(cx: CommandCx, (): ()) -> Result<(), UserError> {
    punish::ensure_role(cx.ctx(), punish::RestrictionRole::Timeout, None).await?;
    Ok(())
}

async fn register_explicit(cx: CommandCx, (): ()) -> Result<(), UserError> {
    punish::ensure_role(
        cx.ctx(),
        punish::RestrictionRole::Timeout,
        Some(Id::new(TIMEOUT_ROLE)),
    )
    .await?;
    Ok(())
}

async fn migrate_voice(cx: CommandCx, (): ()) -> Result<(), UserError> {
    punish::ensure_role(cx.ctx(), punish::RestrictionRole::Voice, None).await?;
    Ok(())
}

async fn voice(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let settings = Settings {
        voice: Method::Role,
        voice_role: Some(Id::new(TIMEOUT_ROLE)),
        ..settings()
    };
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings)
        .voice_mute(Id::new(TARGET), Some(Duration::from_secs(3600)), None)
        .await?;
    Ok(())
}

async fn unvoice(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let settings = Settings {
        voice: Method::Role,
        voice_role: Some(Id::new(TIMEOUT_ROLE)),
        ..settings()
    };
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings)
        .voice_unmute(Id::new(TARGET), None)
        .await?;
    Ok(())
}

async fn auto_settings(
    cx: &CommandCx,
    which: punish::RestrictionRole,
) -> Result<Settings, UserError> {
    let role = punish::ensure_role(cx.ctx(), which, None).await?;
    let mut settings = settings();
    settings.timeout_role = None;
    settings.strip = Strip {
        remove: RoleSelection::All,
        restore: RoleSelection::All,
    };
    match which {
        punish::RestrictionRole::Timeout => settings.timeout_role = Some(role),
        punish::RestrictionRole::Voice => {
            settings.voice = Method::Role;
            settings.voice_role = Some(role);
        }
        punish::RestrictionRole::Quarantine => unreachable!(),
    }
    Ok(settings)
}

async fn auto_long(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let settings = auto_settings(&cx, punish::RestrictionRole::Timeout).await?;
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings)
        .timeout(Id::new(TARGET), Duration::from_secs(3600), None)
        .await?;
    Ok(())
}

async fn auto_short(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let settings = auto_settings(&cx, punish::RestrictionRole::Timeout).await?;
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings)
        .timeout(Id::new(TARGET), Duration::from_mins(30), None)
        .await?;
    Ok(())
}

async fn auto_lift(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let settings = auto_settings(&cx, punish::RestrictionRole::Timeout).await?;
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings)
        .lift_timeout(Id::new(TARGET), None)
        .await?;
    Ok(())
}

async fn auto_voice(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let settings = auto_settings(&cx, punish::RestrictionRole::Voice).await?;
    Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings)
        .voice_mute(Id::new(TARGET), Some(Duration::from_secs(3600)), None)
        .await?;
    Ok(())
}

async fn seed_shared_restore(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let role = cx.store().get::<u64>("test-role").await?.unwrap();
    cx.ctx()
        .restrictions()
        .set("stripped:500", &vec![role])
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
        .command(Command::<()>::new("strip").handler(strip))
        .command(Command::<()>::new("strip_legacy").handler(strip_legacy))
        .command(Command::<()>::new("migrate").handler(migrate))
        .command(Command::<()>::new("register_explicit").handler(register_explicit))
        .command(Command::<()>::new("migrate_voice").handler(migrate_voice))
        .command(Command::<()>::new("voice").handler(voice))
        .command(Command::<()>::new("auto_long").handler(auto_long))
        .command(Command::<()>::new("auto_lift").handler(auto_lift))
        .command(Command::<()>::new("auto_voice").handler(auto_voice));
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
        .command(Command::<()>::new("lift").handler(lift))
        .command(Command::<()>::new("unvoice").handler(unvoice))
        .command(Command::<()>::new("auto_short").handler(auto_short))
        .command(Command::<()>::new("seed_shared_restore").handler(seed_shared_restore));
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
async fn a_legacy_timeout_role_is_removed_by_another_module_after_migration() {
    let first = Harness::new(alpha()).unwrap();
    let second = first.sibling(beta()).unwrap();
    first.api().set_roles(
        Id::new(GUILD),
        vec![role(701, "Mute", 1), role(TIMEOUT_ROLE, "Mute", 1)],
    );
    first
        .api()
        .set_member(Id::new(GUILD), member(TARGET, &[701]));
    first
        .store()
        .set("auto-role:timeout", &701_u64)
        .await
        .unwrap();
    first.invoke(Invocation::new("migrate")).await.unwrap();

    second
        .invoke(Invocation::new("lift").user(MODERATOR))
        .await
        .unwrap();
    assert!(first.api().actions().iter().any(|action| matches!(
        action, Action::RemoveRole { user_id, role_id, .. }
            if user_id.get() == TARGET && role_id.get() == 701
    )));
}

#[tokio::test]
async fn a_lift_skips_a_legacy_role_the_member_does_not_have() {
    let first = Harness::new(alpha()).unwrap();
    let second = first.sibling(beta()).unwrap();
    first.api().set_roles(
        Id::new(GUILD),
        vec![role(701, "Mute", 1), role(TIMEOUT_ROLE, "Mute", 1)],
    );
    first.api().set_member(Id::new(GUILD), member(TARGET, &[]));
    first
        .store()
        .set("auto-role:timeout", &701_u64)
        .await
        .unwrap();
    first.invoke(Invocation::new("migrate")).await.unwrap();
    first
        .api()
        .fail_role(Id::new(701), DiscordError::Forbidden("role".into()));

    second
        .invoke(Invocation::new("lift").user(MODERATOR))
        .await
        .unwrap();
    assert!(!first.api().actions().iter().any(|action| matches!(
        action, Action::RemoveRole { user_id, role_id, .. }
            if user_id.get() == TARGET && role_id.get() == 701
    )));
}

#[tokio::test]
async fn a_new_timeout_timer_also_removes_the_legacy_role() {
    let first = Harness::new(alpha()).unwrap();
    first.api().set_roles(
        Id::new(GUILD),
        vec![role(701, "Mute", 1), role(TIMEOUT_ROLE, "Mute", 1)],
    );
    first
        .api()
        .set_member(Id::new(GUILD), member(TARGET, &[701]));
    first
        .store()
        .set("auto-role:timeout", &701_u64)
        .await
        .unwrap();
    first.invoke(Invocation::new("migrate")).await.unwrap();
    first
        .invoke(Invocation::new("strip").user(MODERATOR))
        .await
        .unwrap();
    first.advance(Duration::from_secs(3600));
    let results = first.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert!(first.api().actions().iter().any(|action| matches!(
        action, Action::RemoveRole { user_id, role_id, .. }
            if user_id.get() == TARGET && role_id.get() == 701
    )));
}

#[tokio::test]
async fn a_legacy_voice_role_is_removed_by_a_manual_lift_and_a_new_timer() {
    for by_timer in [false, true] {
        let first = Harness::new(alpha()).unwrap();
        let second = first.sibling(beta()).unwrap();
        first.api().set_roles(
            Id::new(GUILD),
            vec![role(701, "VC mute", 1), role(TIMEOUT_ROLE, "VC mute", 1)],
        );
        first
            .api()
            .set_member(Id::new(GUILD), member(TARGET, &[701]));
        first
            .store()
            .set("auto-role:vcmute", &701_u64)
            .await
            .unwrap();
        first
            .invoke(Invocation::new("migrate_voice"))
            .await
            .unwrap();
        first
            .invoke(Invocation::new("voice").user(MODERATOR))
            .await
            .unwrap();
        if by_timer {
            first.advance(Duration::from_secs(3600));
            let results = first.run_due_jobs().await;
            assert!(results.iter().all(Result::is_ok), "{results:?}");
        } else {
            second
                .invoke(Invocation::new("unvoice").user(MODERATOR))
                .await
                .unwrap();
        }
        assert!(
            first.api().actions().iter().any(|action| matches!(
                action, Action::RemoveRole { user_id, role_id, .. }
                    if user_id.get() == TARGET && role_id.get() == 701
            )),
            "by_timer: {by_timer}"
        );
    }
}

#[tokio::test]
async fn a_legacy_restriction_role_is_not_stripped_and_remembered_as_an_ordinary_role() {
    let first = Harness::new(alpha()).unwrap();
    first.api().set_roles(
        Id::new(GUILD),
        vec![
            role(BOT_ROLE, "Bot", 50),
            role(701, "Mute", 1),
            role(TIMEOUT_ROLE, "Mute", 1),
        ],
    );
    first
        .api()
        .set_member(Id::new(GUILD), member(BOT, &[BOT_ROLE]));
    first
        .api()
        .set_member(Id::new(GUILD), member(TARGET, &[701]));
    first
        .store()
        .set("auto-role:timeout", &701_u64)
        .await
        .unwrap();
    first.invoke(Invocation::new("migrate")).await.unwrap();
    first
        .invoke(Invocation::new("strip_legacy").user(MODERATOR))
        .await
        .unwrap();
    assert!(!first.api().actions().iter().any(|action| matches!(
        action, Action::RemoveRole { user_id, role_id, .. }
            if user_id.get() == TARGET && role_id.get() == 701
    )));
}

#[tokio::test]
async fn a_timeout_by_one_module_is_lifted_by_another_and_gives_the_roles_back() {
    let first = Harness::new(alpha()).unwrap();
    let second = first.sibling(beta()).unwrap();
    first.api().set_roles(
        Id::new(GUILD),
        vec![
            role(BOT_ROLE, "Bot", 50),
            role(TIMEOUT_ROLE, "Mute", 1),
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

fn cross_module_harness() -> (Harness, Harness) {
    let first = Harness::new(alpha()).unwrap();
    let second = first.sibling(beta()).unwrap();
    first
        .api()
        .set_roles(Id::new(GUILD), vec![role(BOT_ROLE, "Bot", 50)]);
    first
        .api()
        .set_member(Id::new(GUILD), member(BOT, &[BOT_ROLE]));
    first.api().set_member(Id::new(GUILD), member(TARGET, &[]));
    (first, second)
}

fn removed(harness: &Harness, role: u64) -> usize {
    harness
        .api()
        .actions()
        .iter()
        .filter(|action| {
            matches!(
                action, Action::RemoveRole { user_id, role_id, .. }
                    if user_id.get() == TARGET && role_id.get() == role
            )
        })
        .count()
}

#[tokio::test]
async fn cross_module_timeout_roles_are_not_stripped_or_restored_after_timers() {
    let (moderation, automod) = cross_module_harness();
    moderation
        .invoke(Invocation::new("auto_long").user(MODERATOR))
        .await
        .unwrap();
    let m = moderation
        .store()
        .get::<u64>("auto-role:timeout")
        .await
        .unwrap()
        .unwrap();
    moderation
        .api()
        .set_member(Id::new(GUILD), member(TARGET, &[m]));

    automod
        .invoke(Invocation::new("auto_short").user(MODERATOR))
        .await
        .unwrap();
    let a = automod
        .store()
        .get::<u64>("auto-role:timeout")
        .await
        .unwrap()
        .unwrap();
    assert_ne!(m, a);
    assert_eq!(removed(&moderation, m), 0);

    // The fake API records role changes but does not change the member.
    moderation
        .api()
        .set_member(Id::new(GUILD), member(TARGET, &[m, a]));
    moderation
        .invoke(Invocation::new("auto_lift").user(MODERATOR))
        .await
        .unwrap();
    assert_eq!(removed(&moderation, m), 1);
    automod.advance(Duration::from_mins(30));
    let results = automod.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(removed(&automod, a), 0);
    automod.advance(Duration::from_mins(30));
    let results = automod.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert!(removed(&automod, a) >= 1);
    assert_eq!(given(&moderation).iter().filter(|&&id| id == m).count(), 1);
}

#[tokio::test]
async fn cross_module_restriction_in_old_restore_list_is_not_given_back() {
    let (moderation, automod) = cross_module_harness();
    moderation
        .invoke(Invocation::new("auto_long").user(MODERATOR))
        .await
        .unwrap();
    let m = moderation
        .store()
        .get::<u64>("auto-role:timeout")
        .await
        .unwrap()
        .unwrap();
    automod
        .invoke(Invocation::new("auto_short").user(MODERATOR))
        .await
        .unwrap();
    automod.store().set("test-role", &m).await.unwrap();
    automod
        .invoke(Invocation::new("seed_shared_restore"))
        .await
        .unwrap();
    moderation
        .invoke(Invocation::new("auto_lift").user(MODERATOR))
        .await
        .unwrap();
    automod.advance(Duration::from_mins(30));
    assert!(automod.run_due_jobs().await.iter().all(Result::is_ok));
    automod.advance(Duration::from_mins(30));
    assert!(automod.run_due_jobs().await.iter().all(Result::is_ok));
    assert_eq!(given(&moderation).iter().filter(|&&id| id == m).count(), 1);
}

#[tokio::test]
async fn cross_module_voice_strip_keeps_automod_timeout_role() {
    let (moderation, automod) = cross_module_harness();
    automod
        .invoke(Invocation::new("auto_short").user(MODERATOR))
        .await
        .unwrap();
    let a = automod
        .store()
        .get::<u64>("auto-role:timeout")
        .await
        .unwrap()
        .unwrap();
    moderation
        .api()
        .set_member(Id::new(GUILD), member(TARGET, &[a]));
    moderation
        .invoke(Invocation::new("auto_voice").user(MODERATOR))
        .await
        .unwrap();
    assert_eq!(removed(&moderation, a), 0);
}

#[tokio::test]
async fn cross_module_explicit_restriction_role_is_not_stripped() {
    let (moderation, automod) = cross_module_harness();
    moderation.api().set_roles(
        Id::new(GUILD),
        vec![
            role(BOT_ROLE, "Bot", 50),
            role(TIMEOUT_ROLE, "Configured", 1),
        ],
    );
    moderation
        .invoke(Invocation::new("register_explicit"))
        .await
        .unwrap();
    moderation
        .api()
        .set_member(Id::new(GUILD), member(TARGET, &[TIMEOUT_ROLE]));
    automod
        .invoke(Invocation::new("auto_short").user(MODERATOR))
        .await
        .unwrap();
    assert_eq!(removed(&moderation, TIMEOUT_ROLE), 0);
}
