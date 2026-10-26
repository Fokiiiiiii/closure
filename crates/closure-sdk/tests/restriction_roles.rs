//! The dedicated roles of the role method: created on first use or found by name, set up in every
//! channel by the sync job, and given to channels created later.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

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
    serde_json::from_value(json!({
        "id": id.to_string(), "name": name, "color": 0, "hoist": false, "managed": false,
        "mentionable": false, "permissions": "0", "position": 1, "flags": 0,
        "icon": null, "unicode_emoji": null, "tags": null,
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

/// A module that sets the two dedicated roles up when its configuration changes, and sets the
/// remembered ones on a channel that is created. `configured` is the timeout role the server named.
fn module(configured: Option<Id<RoleMarker>>) -> ModuleBuilder {
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
            move |ctx, _| async move {
                ensure_role(&ctx, RestrictionRole::Timeout, configured)
                    .await
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                ensure_role(&ctx, RestrictionRole::Voice, None)
                    .await
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                Ok(Flow::Continue)
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
    harness.api().set_channels(
        Id::new(GUILD),
        vec![channel(1, &[]), channel(2, &[(501, 0, 0)]), channel(3, &[])],
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
