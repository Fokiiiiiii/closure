//! The dedicated roles of the role method: created on first use or found by name, set up in every
//! channel by the sync job, and given to channels created later.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use closure_core::action::{Action, OverwriteTarget};
use closure_core::error::DiscordError;
use closure_core::event::Event;
use closure_sdk::punish::{self, RestrictionRole, channel_created, ensure_role};
use closure_sdk::testing::Harness;
use closure_sdk::{Capability, EventKind, Flow, ModuleBuilder, ModuleError, Subscription};
use serde_json::json;
use twilight_model::channel::Channel;
use twilight_model::guild::{Permissions, Role};
use twilight_model::id::Id;
use twilight_model::id::marker::RoleMarker;

const GUILD: u64 = 1;
const LOG: u64 = 9;

fn role(id: u64, name: &str) -> Role {
    role_with(id, name, Permissions::empty(), false)
}

/// A role with the given permissions, and managed or not.
fn role_with(id: u64, name: &str, permissions: Permissions, managed: bool) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": name, "color": 0, "hoist": false, "managed": managed,
        "mentionable": false, "permissions": permissions.bits().to_string(), "position": 1,
        "flags": 0, "icon": null, "unicode_emoji": null, "tags": null,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    }))
    .unwrap()
}

/// A text channel with the given overwrites: (target id, 0 for a role or 1 for a member, allow).
fn channel(id: u64, overwrites: &[(u64, u8, u64)]) -> Channel {
    let overwrites: Vec<_> = overwrites
        .iter()
        .map(|(target, kind, allow)| {
            json!({ "id": target.to_string(), "type": kind, "allow": allow.to_string(), "deny": "0" })
        })
        .collect();
    serde_json::from_value(json!({
        "id": id.to_string(), "type": 0, "guild_id": GUILD.to_string(),
        "name": format!("channel-{id}"), "permission_overwrites": overwrites
    }))
    .unwrap()
}

/// A text channel with overwrites of (target id, 0 for a role or 1 for a member, allow, deny).
fn channel_with(id: u64, overwrites: &[(u64, u8, Permissions, Permissions)]) -> Channel {
    let overwrites: Vec<_> = overwrites
        .iter()
        .map(|(target, kind, allow, deny)| {
            json!({
                "id": target.to_string(), "type": kind,
                "allow": allow.bits().to_string(), "deny": deny.bits().to_string()
            })
        })
        .collect();
    serde_json::from_value(json!({
        "id": id.to_string(), "type": 0, "guild_id": GUILD.to_string(),
        "name": format!("channel-{id}"), "permission_overwrites": overwrites
    }))
    .unwrap()
}

/// The timeout role's denies, which the sync sets in a channel.
fn timeout_denies() -> Permissions {
    Permissions::SEND_MESSAGES
        | Permissions::SEND_MESSAGES_IN_THREADS
        | Permissions::CREATE_PUBLIC_THREADS
        | Permissions::CREATE_PRIVATE_THREADS
        | Permissions::ADD_REACTIONS
        | Permissions::SPEAK
        | Permissions::STREAM
        | Permissions::REQUEST_TO_SPEAK
}

/// The configured timeout role of a module, which a test may change between two setups.
type Configured = Arc<Mutex<Option<Id<RoleMarker>>>>;

/// A module that sets the two dedicated roles up when its configuration changes, and sets the
/// remembered ones on a channel that is created. `configured` is the timeout role the server named.
fn module(configured: Option<Id<RoleMarker>>) -> ModuleBuilder {
    module_with(Arc::new(Mutex::new(configured)))
}

fn module_with(configured: Configured) -> ModuleBuilder {
    let builder = ModuleBuilder::new("roles", "Roles", "0.1.0")
        .unwrap()
        .capability(Capability::ManageRoles)
        .capability(Capability::ManageChannels)
        .capability(Capability::ReadGuild)
        .capability(Capability::SendMessages)
        .capability(Capability::Storage)
        .capability(Capability::Schedule)
        .config(json!({ "type": "object", "properties": {} }), json!({}));
    punish::jobs(builder)
        .on(
            Subscription::new(EventKind::ConfigChanged),
            move |ctx, _| {
                let configured = Arc::clone(&configured);
                async move {
                    let configured = *configured.lock().unwrap_or_else(PoisonError::into_inner);
                    ensure_role(&ctx, RestrictionRole::Timeout, configured)
                        .await
                        .map_err(|error| ModuleError::other(error.to_string()))?;
                    ensure_role(&ctx, RestrictionRole::Voice, None)
                        .await
                        .map_err(|error| ModuleError::other(error.to_string()))?;
                    Ok(Flow::Continue)
                }
            },
        )
        .on(
            Subscription::new(EventKind::ChannelCreated),
            |ctx, event| async move {
                if let Event::ChannelCreated(channel) = event {
                    channel_created(&ctx, channel.id).await?;
                }
                Ok(Flow::Continue)
            },
        )
}

fn created_names(harness: &Harness) -> Vec<String> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::CreateRole { spec, .. } => Some(spec.name),
            _ => None,
        })
        .collect()
}

/// Every overwrite the module set on a role: (channel id, role id, deny).
fn overwrites(harness: &Harness) -> Vec<(u64, u64, Permissions)> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SetPermissionOverwrite {
                channel_id,
                overwrite,
                ..
            } => match overwrite.target {
                OverwriteTarget::Role(role) => Some((channel_id.get(), role.get(), overwrite.deny)),
                OverwriteTarget::Member(_) => None,
            },
            _ => None,
        })
        .collect()
}

async fn run_all_jobs(harness: &Harness) {
    for _ in 0..50 {
        if harness.run_due_jobs().await.is_empty() {
            break;
        }
    }
}

#[tokio::test]
async fn a_missing_role_is_created_with_no_permissions_and_its_sync_is_scheduled() {
    let harness = Harness::new(module(None)).unwrap();
    harness.config_changed().await.unwrap();
    assert_eq!(created_names(&harness), ["Timed out", "Voice muted"]);
    let actions = harness.api().actions();
    let created = actions
        .iter()
        .filter_map(|action| match action {
            Action::CreateRole { spec, .. } => Some(spec),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        created
            .iter()
            .all(|spec| spec.permissions.is_empty() && !spec.hoist && !spec.mentionable)
    );
    assert_eq!(harness.jobs(), 2, "one sync per dedicated role");
}

#[tokio::test]
async fn a_configured_role_that_exists_is_used_and_its_channels_are_left_alone() {
    let harness = Harness::new(module(Some(Id::new(42)))).unwrap();
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(42, "Mods' mute")]);
    harness
        .api()
        .set_channels(Id::new(GUILD), vec![channel(5, &[])]);
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;
    assert_eq!(
        created_names(&harness),
        ["Voice muted"],
        "the configured role is not created"
    );
    assert!(
        overwrites(&harness).iter().all(|(_, role, _)| *role != 42),
        "the admin's role keeps its channel permissions"
    );
}

#[tokio::test]
async fn a_role_found_by_name_is_used_and_not_created_again() {
    let harness = Harness::new(module(None)).unwrap();
    harness.api().set_roles(
        Id::new(GUILD),
        vec![role(700, "timed out"), role(701, "Voice muted")],
    );
    harness.config_changed().await.unwrap();
    harness.config_changed().await.unwrap();
    assert!(
        created_names(&harness).is_empty(),
        "both roles already exist by name"
    );
}

#[tokio::test]
async fn a_second_setup_does_not_create_a_second_role() {
    let harness = Harness::new(module(None)).unwrap();
    harness.config_changed().await.unwrap();
    harness.config_changed().await.unwrap();
    assert_eq!(created_names(&harness), ["Timed out", "Voice muted"]);
}

#[tokio::test]
async fn a_refusal_to_create_the_role_is_a_clear_error() {
    let harness = Harness::new(module(None)).unwrap();
    harness
        .api()
        .fail_with(DiscordError::Forbidden("Missing Permissions".into()));
    let error = harness.config_changed().await.unwrap_err();
    assert!(error.to_string().contains("refused"), "{error}");
}

#[tokio::test]
async fn the_sync_denies_speech_where_a_channel_has_no_overwrite_and_skips_the_rest() {
    let harness = Harness::new(module(None)).unwrap();
    harness.set_log_channel(LOG);
    // An explicit role: the created ids depend on how many calls came before them.
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(700, "Timed out")]);
    harness.api().set_channels(
        Id::new(GUILD),
        vec![
            channel(1, &[]),
            // The timeout role already denies everything in this channel: nothing to set.
            channel_with(2, &[(700, 0, Permissions::empty(), timeout_denies())]),
            channel(3, &[]),
        ],
    );
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;

    let set = overwrites(&harness);
    let timeout = set
        .iter()
        .filter(|(_, _, deny)| deny.contains(Permissions::SEND_MESSAGES))
        .map(|(channel, _, _)| *channel)
        .collect::<Vec<_>>();
    assert_eq!(
        timeout,
        [1, 3],
        "the channel that already has the overwrite is left alone"
    );
    let voice = set
        .iter()
        .filter(|(_, _, deny)| deny.contains(Permissions::USE_VAD))
        .map(|(channel, _, _)| *channel)
        .collect::<Vec<_>>();
    assert_eq!(voice, [1, 2, 3]);
    let deny = set
        .iter()
        .find(|(_, _, deny)| deny.contains(Permissions::SEND_MESSAGES))
        .unwrap()
        .2;
    assert!(
        deny.contains(Permissions::REQUEST_TO_SPEAK | Permissions::STREAM | Permissions::SPEAK)
    );
    assert!(
        !deny.contains(Permissions::VIEW_CHANNEL),
        "the role does not hide channels"
    );
    assert!(
        harness.api().actions().iter().any(|action| matches!(
            action,
            Action::SendMessage {
                content: Some(_),
                ..
            }
        )),
        "the log says how the sync went"
    );
}

#[tokio::test]
async fn a_channel_discord_refuses_is_skipped_and_the_sync_goes_on() {
    let harness = Harness::new(module(None)).unwrap();
    harness.api().set_channels(
        Id::new(GUILD),
        vec![channel(1, &[]), channel(2, &[]), channel(3, &[])],
    );
    harness.config_changed().await.unwrap();
    // The sync's first call is the channel list; the second is the first overwrite.
    harness
        .api()
        .fail_nth(2, DiscordError::Forbidden("Missing Permissions".into()));
    run_all_jobs(&harness).await;
    let timeout = overwrites(&harness)
        .iter()
        .filter(|(_, _, deny)| deny.contains(Permissions::SEND_MESSAGES))
        .count();
    assert_eq!(timeout, 2, "the other two channels were still set up");
}

#[tokio::test]
async fn the_sync_runs_in_batches_of_ten_until_every_channel_is_done() {
    let harness = Harness::new(module(None)).unwrap();
    let channels: Vec<_> = (100..112).map(|id| channel(id, &[])).collect();
    harness.api().set_channels(Id::new(GUILD), channels);
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;
    let timeout = overwrites(&harness)
        .iter()
        .filter(|(_, _, deny)| deny.contains(Permissions::SEND_MESSAGES))
        .count();
    assert_eq!(
        timeout, 12,
        "every channel of the guild is set up, in two runs"
    );
}

#[tokio::test]
async fn a_running_sync_is_not_started_again_by_a_later_change() {
    let harness = Harness::new(module(None)).unwrap();
    let channels: Vec<_> = (100..112).map(|id| channel(id, &[])).collect();
    harness.api().set_channels(Id::new(GUILD), channels);
    harness.config_changed().await.unwrap();
    // One run sets the first batch of each sync; the rest waits for the next runs.
    harness.run_due_jobs().await;
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;
    let timeout = overwrites(&harness)
        .iter()
        .filter(|(_, _, deny)| deny.contains(Permissions::SEND_MESSAGES))
        .count();
    assert_eq!(
        timeout, 12,
        "a restarted sync would set the first channels again"
    );
}

#[tokio::test]
async fn a_new_channel_gets_the_remembered_roles_overwrites() {
    let harness = Harness::new(module(None)).unwrap();
    harness.config_changed().await.unwrap();
    harness.api().add_channel(Id::new(50), Some(Id::new(GUILD)));
    harness.channel_created(50).await.unwrap();
    let on_new: Vec<_> = overwrites(&harness)
        .into_iter()
        .filter(|(channel, _, _)| *channel == 50)
        .collect();
    assert_eq!(on_new.len(), 2, "one overwrite for each dedicated role");
}

/// Every overwrite the module set on one role in one channel: (allow, deny), in order.
fn specs_of(harness: &Harness, channel: u64, role: u64) -> Vec<(Permissions, Permissions)> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SetPermissionOverwrite {
                channel_id,
                overwrite,
                ..
            } => match overwrite.target {
                OverwriteTarget::Role(target)
                    if channel_id.get() == channel && target.get() == role =>
                {
                    Some((overwrite.allow, overwrite.deny))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// How many overwrites were attempted on one role, the failed ones included.
fn attempts_on(harness: &Harness, role: u64) -> usize {
    harness
        .api()
        .actions()
        .iter()
        .filter(|action| match action {
            Action::SetPermissionOverwrite { overwrite, .. } => {
                overwrite.target == OverwriteTarget::Role(Id::new(role))
            }
            _ => false,
        })
        .count()
}

/// The texts of the messages the module posted to the log.
fn sent_texts(harness: &Harness) -> Vec<String> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                content: Some(text),
                ..
            } => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_unsafe_role_with_a_restriction_name_is_ignored_and_a_safe_one_is_created() {
    let harness = Harness::new(module(None)).unwrap();
    harness.api().set_roles(
        Id::new(GUILD),
        vec![
            role_with(700, "Timed out", Permissions::ADMINISTRATOR, false),
            role_with(701, "voice muted", Permissions::empty(), true),
        ],
    );
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;
    assert_eq!(created_names(&harness), ["Timed out", "Voice muted"]);
    assert!(
        overwrites(&harness)
            .iter()
            .all(|(_, role, _)| *role != 700 && *role != 701),
        "the unsafe roles are never set up or given"
    );
}

#[tokio::test]
async fn a_remembered_role_that_gains_permissions_is_replaced() {
    let harness = Harness::new(module(None)).unwrap();
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(700, "Timed out")]);
    harness.config_changed().await.unwrap();
    assert_eq!(created_names(&harness), ["Voice muted"], "700 is adopted");
    harness.api().set_roles(
        Id::new(GUILD),
        vec![
            role_with(700, "Timed out", Permissions::ADMINISTRATOR, false),
            role(800, "Voice muted"),
        ],
    );
    harness.config_changed().await.unwrap();
    assert_eq!(
        created_names(&harness),
        ["Voice muted", "Timed out"],
        "the remembered 700 is no longer safe, so a new role is made"
    );
}

#[tokio::test]
async fn two_setups_at_the_same_time_create_one_role_each() {
    let harness = Harness::new(module(None)).unwrap();
    let (first, second) = tokio::join!(harness.config_changed(), harness.config_changed());
    first.unwrap();
    second.unwrap();
    assert_eq!(created_names(&harness), ["Timed out", "Voice muted"]);
}

#[tokio::test]
async fn a_channel_created_later_keeps_the_denies_the_role_already_has_there() {
    let harness = Harness::new(module(None)).unwrap();
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(700, "Timed out")]);
    harness.config_changed().await.unwrap();
    harness.api().set_channels(
        Id::new(GUILD),
        vec![channel_with(
            50,
            &[(700, 0, Permissions::empty(), Permissions::VIEW_CHANNEL)],
        )],
    );
    harness.channel_created(50).await.unwrap();
    let specs = specs_of(&harness, 50, 700);
    assert_eq!(specs.len(), 1, "{specs:?}");
    let deny = specs[0].1;
    assert!(
        deny.contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES),
        "the existing deny stays: {deny:?}"
    );
}

#[tokio::test]
async fn a_channel_created_later_leaves_alone_an_auto_role_an_explicit_role_has_replaced() {
    let configured: Configured = Arc::new(Mutex::new(None));
    let harness = Harness::new(module_with(Arc::clone(&configured))).unwrap();
    harness.api().set_roles(
        Id::new(GUILD),
        vec![role(700, "Timed out"), role(42, "Mods' mute")],
    );
    harness.config_changed().await.unwrap();
    *configured.lock().unwrap_or_else(PoisonError::into_inner) = Some(Id::new(42));
    harness.config_changed().await.unwrap();
    harness.api().add_channel(Id::new(50), Some(Id::new(GUILD)));
    harness.channel_created(50).await.unwrap();
    assert!(
        specs_of(&harness, 50, 700).is_empty(),
        "the auto role is no longer in use"
    );
    assert!(
        specs_of(&harness, 50, 42).is_empty(),
        "the explicit role is not touched"
    );
}

#[tokio::test]
async fn a_channel_the_sync_could_not_set_up_is_set_up_by_the_retry() {
    let harness = Harness::new(module(None)).unwrap();
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(700, "Timed out")]);
    harness.api().set_channels(
        Id::new(GUILD),
        vec![channel(1, &[]), channel(2, &[]), channel(3, &[])],
    );
    harness.api().fail_role(
        Id::new(700),
        DiscordError::Forbidden("Missing Permissions".into()),
    );
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;
    assert_eq!(
        harness.jobs(),
        1,
        "the sync is not marked done: a retry waits an hour"
    );
    harness.api().recover_roles();
    let before = attempts_on(&harness, 700);
    harness.advance(Duration::from_secs(3_600));
    run_all_jobs(&harness).await;
    assert_eq!(
        attempts_on(&harness, 700) - before,
        3,
        "every channel is tried again"
    );
    for channel in [1, 2, 3] {
        let last = specs_of(&harness, channel, 700).last().map(|spec| spec.1);
        assert!(
            last.is_some_and(|deny| deny.contains(Permissions::SEND_MESSAGES)),
            "channel {channel} is set up by the retry"
        );
    }
}

#[tokio::test]
async fn a_sync_that_keeps_failing_is_retried_three_times_and_then_noted() {
    let harness = Harness::new(module(None)).unwrap();
    harness.set_log_channel(LOG);
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(700, "Timed out")]);
    harness
        .api()
        .set_channels(Id::new(GUILD), vec![channel(1, &[]), channel(2, &[])]);
    harness.api().fail_role(
        Id::new(700),
        DiscordError::Forbidden("Missing Permissions".into()),
    );
    harness.config_changed().await.unwrap();
    for _ in 0..4 {
        run_all_jobs(&harness).await;
        harness.advance(Duration::from_secs(3_600));
    }
    assert_eq!(
        attempts_on(&harness, 700),
        8,
        "one run and three retries of two channels"
    );
    assert_eq!(harness.jobs(), 0, "no retry after the third");
    let texts = sent_texts(&harness);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("2 channels could not be set up")),
        "{texts:?}"
    );
}

#[tokio::test]
async fn an_allow_of_everyone_is_not_a_way_around_the_restriction() {
    let harness = Harness::new(module(None)).unwrap();
    harness.set_log_channel(LOG);
    harness.api().set_channels(
        Id::new(GUILD),
        vec![channel(1, &[(GUILD, 0, Permissions::SEND_MESSAGES.bits())])],
    );
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;
    let texts = sent_texts(&harness);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("0 channels let another role or a member speak")),
        "{texts:?}"
    );
}

#[tokio::test]
async fn a_member_allow_is_a_way_around_the_restriction() {
    let harness = Harness::new(module(None)).unwrap();
    harness.set_log_channel(LOG);
    harness.api().set_channels(
        Id::new(GUILD),
        vec![channel(1, &[(9, 1, Permissions::SEND_MESSAGES.bits())])],
    );
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;
    let texts = sent_texts(&harness);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("1 channels let another role or a member speak")),
        "{texts:?}"
    );
}

#[tokio::test]
async fn the_sync_keeps_the_denies_and_drops_the_allows_of_the_role_it_sets() {
    let harness = Harness::new(module(None)).unwrap();
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(700, "Timed out")]);
    harness.api().set_channels(
        Id::new(GUILD),
        vec![channel_with(
            1,
            &[(
                700,
                0,
                Permissions::SEND_MESSAGES,
                Permissions::VIEW_CHANNEL,
            )],
        )],
    );
    harness.config_changed().await.unwrap();
    run_all_jobs(&harness).await;
    let specs = specs_of(&harness, 1, 700);
    let (allow, deny) = *specs.last().unwrap();
    assert!(
        deny.contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES),
        "{deny:?}"
    );
    assert!(!allow.contains(Permissions::SEND_MESSAGES), "{allow:?}");
}
