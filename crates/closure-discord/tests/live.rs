//! Tests against the real Discord API, using the developer's own test application and server.
//!
//! They are `#[ignore]`d so a plain `cargo test` never needs a token. Run them with:
//!
//! ```text
//! cargo test -p closure-discord --test live -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Credentials come from the environment or the git-ignored `.env` at the repository root:
//! `DISCORD_TOKEN`, `DISCORD_APPLICATION_ID` and `TEST_GUILD_ID`. Tests whose name starts with
//! `in_guild_` additionally need the bot to have been invited to that server.

// Helpers in an integration test are outside `#[test]` functions, where the workspace's
// `unwrap`/`expect` ban would otherwise apply; a panic is the right way for a test to fail.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use closure_core::action::{
    Action, ChannelKind, ChannelPatch, NewChannel, NewRole, Outcome, Reason, RolePatch,
};
use closure_core::client::DiscordApi;
use closure_core::error::DiscordError;
use closure_core::registry::ModuleRegistry;
use closure_core::testing::StaticSettings;
use closure_discord::{
    CommandScope, Dispatcher, DispatcherConfig, GatewayConfig, NoHooks, TwilightApi,
    build_commands, gateway_intents, new_cache, register_commands, start_gateway,
};
use closure_i18n::TranslatorBuilder;
use tokio_util::sync::CancellationToken;
use twilight_http::Client;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::{ApplicationMarker, ChannelMarker, GuildMarker};

fn setting(key: &str) -> String {
    if let Ok(value) = std::env::var(key) {
        return value;
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../.env");
    let contents = std::fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("{key} is not set and {path} could not be read"));
    contents
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(name, _)| name.trim() == key)
        .map_or_else(
            || panic!("{key} is missing from {path}"),
            |(_, value)| value.trim().to_owned(),
        )
}

fn id<M>(key: &str) -> Id<M> {
    Id::new(setting(key).parse().expect("a numeric id"))
}

struct Live {
    http: Arc<Client>,
    api: TwilightApi,
    guild: Id<GuildMarker>,
    application: Id<ApplicationMarker>,
}

fn live() -> Live {
    let http = Arc::new(Client::new(setting("DISCORD_TOKEN")));
    let application = id("DISCORD_APPLICATION_ID");
    Live {
        api: TwilightApi::new(Arc::clone(&http), application),
        http,
        guild: id("TEST_GUILD_ID"),
        application,
    }
}

fn reason(text: &str) -> Reason {
    Reason::new(text).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "talks to the real Discord API"]
async fn gateway_connects_and_closes_cleanly() {
    let live = live();
    let registry = Arc::new(ModuleRegistry::new(vec![]).unwrap());
    let dispatcher = Dispatcher::start(
        Arc::clone(&registry),
        Arc::new(StaticSettings::new([])),
        Arc::new(TwilightApi::new(Arc::clone(&live.http), live.application)),
        Arc::new(TranslatorBuilder::new().unwrap().build()),
        DispatcherConfig::default(),
    )
    .await
    .unwrap();

    let shutdown = CancellationToken::new();
    let handle = start_gateway(
        &live.http,
        GatewayConfig {
            token: setting("DISCORD_TOKEN"),
            intents: gateway_intents(&registry),
            latency: Arc::default(),
        },
        new_cache(),
        dispatcher.clone(),
        Arc::new(NoHooks),
        shutdown.clone(),
    )
    .await
    .expect("the gateway should start with a valid token");

    tokio::time::timeout(Duration::from_secs(30), async {
        while !handle.health().all_ready() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("every shard should become ready");
    println!("connected: {} shard(s) ready", handle.health().ready());

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(15), handle.closed())
        .await
        .expect("shards should close after shutdown was requested");
    assert_eq!(handle.health().ready(), 0);
    assert!(dispatcher.shutdown(Duration::from_secs(5)).await);
}

#[tokio::test]
#[ignore = "talks to the real Discord API"]
async fn unknown_targets_are_reported_as_not_found() {
    let live = live();
    let result = live
        .api
        .execute(Action::GetChannel {
            channel_id: Id::<ChannelMarker>::new(1),
        })
        .await;
    assert!(
        matches!(
            result,
            Err(DiscordError::NotFound | DiscordError::Forbidden(_))
        ),
        "{result:?}"
    );
}

#[tokio::test]
#[ignore = "talks to the real Discord API"]
async fn in_guild_server_details_are_readable() {
    let live = live();
    let Outcome::Guild(guild) = live
        .api
        .execute(Action::GetGuild {
            guild_id: live.guild,
        })
        .await
        .unwrap()
    else {
        panic!("expected a guild");
    };
    println!("guild: {} (locale {})", guild.name, guild.preferred_locale);

    let Outcome::Roles(roles) = live
        .api
        .execute(Action::ListRoles {
            guild_id: live.guild,
        })
        .await
        .unwrap()
    else {
        panic!("expected roles");
    };
    assert!(!roles.is_empty(), "every server has at least @everyone");

    live.api
        .execute(Action::ListBans {
            guild_id: live.guild,
            after: None,
            limit: 10,
        })
        .await
        .unwrap();
    live.api
        .execute(Action::GetAuditLog {
            guild_id: live.guild,
            query: closure_core::action::AuditLogQuery {
                limit: Some(1),
                ..Default::default()
            },
        })
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "talks to the real Discord API"]
async fn in_guild_messages_can_be_sent_edited_reacted_to_and_deleted() {
    let live = live();
    let note = reason("closure live test");

    let Outcome::Channel(channel) = live
        .api
        .execute(Action::CreateChannel {
            guild_id: live.guild,
            spec: NewChannel {
                name: "closure-live-test".into(),
                kind: ChannelKind::Text,
                parent_id: None,
                topic: Some("temporary channel created by the closure test-suite".into()),
                nsfw: false,
                slowmode_seconds: 0,
                overwrites: vec![],
            },
            reason: note.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("expected a channel");
    };
    let channel_id = channel.id;

    let outcome = async {
        let Outcome::Message(sent) = live
            .api
            .execute(Action::SendMessage {
                channel_id,
                content: Some("hello from closure @everyone".into()),
                embeds: vec![],
                reply_to: None,
                mentions: closure_core::action::MentionPolicy::None,
            })
            .await?
        else {
            panic!("expected a message");
        };
        assert!(!sent.mention_everyone, "the default policy must not ping");

        live.api
            .execute(Action::EditMessage {
                channel_id,
                message_id: sent.id,
                content: Some("edited".into()),
                embeds: None,
            })
            .await?;
        live.api
            .execute(Action::AddReaction {
                channel_id,
                message_id: sent.id,
                emoji: closure_core::action::Emoji::Unicode {
                    value: "✅".into()
                },
            })
            .await?;
        live.api
            .execute(Action::EditChannel {
                channel_id,
                patch: ChannelPatch {
                    slowmode_seconds: Some(5),
                    ..ChannelPatch::default()
                },
                reason: note.clone(),
            })
            .await?;
        live.api
            .execute(Action::DeleteMessage {
                channel_id,
                message_id: sent.id,
                reason: note.clone(),
            })
            .await
    }
    .await;

    // Always remove the channel, even if a step above failed.
    let cleanup = live
        .api
        .execute(Action::DeleteChannel {
            channel_id,
            reason: note,
        })
        .await;
    outcome.unwrap();
    cleanup.unwrap();
}

#[tokio::test]
#[ignore = "talks to the real Discord API"]
async fn in_guild_roles_can_be_created_edited_and_deleted() {
    let live = live();
    let note = reason("closure live test");

    let Outcome::Role(role) = live
        .api
        .execute(Action::CreateRole {
            guild_id: live.guild,
            spec: NewRole {
                name: "closure-live-test".into(),
                color: 0x00_80_FF,
                permissions: Permissions::empty(),
                hoist: false,
                mentionable: false,
            },
            reason: note.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("expected a role");
    };

    let edited = live
        .api
        .execute(Action::EditRole {
            guild_id: live.guild,
            role_id: role.id,
            patch: RolePatch {
                name: Some("closure-live-test-renamed".into()),
                ..RolePatch::default()
            },
            reason: note.clone(),
        })
        .await;
    let removed = live
        .api
        .execute(Action::DeleteRole {
            guild_id: live.guild,
            role_id: role.id,
            reason: note,
        })
        .await;
    let Outcome::Role(edited) = edited.unwrap() else {
        panic!("expected a role");
    };
    assert_eq!(edited.name, "closure-live-test-renamed");
    removed.unwrap();
}

#[tokio::test]
#[ignore = "talks to the real Discord API"]
async fn in_guild_commands_can_be_registered_and_cleared() {
    let live = live();
    let translator = TranslatorBuilder::new().unwrap().build();
    let registry = ModuleRegistry::new(vec![]).unwrap();
    let commands = build_commands(&registry, &translator).unwrap();
    let published = register_commands(
        &live.http,
        live.application,
        CommandScope::Guild(live.guild),
        &commands,
    )
    .await
    .unwrap();
    assert_eq!(published, commands.len());
}

#[tokio::test]
#[ignore = "talks to the real Discord API"]
async fn in_guild_a_client_bound_to_another_guild_cannot_touch_this_guilds_channels() {
    use closure_core::capability::Capability;
    use closure_core::client::ApiClient;
    use closure_core::module::ModuleId;

    let live = live();
    let note = reason("closure live test");
    let Outcome::Channel(channel) = live
        .api
        .execute(Action::CreateChannel {
            guild_id: live.guild,
            spec: NewChannel {
                name: "closure-scope-test".into(),
                kind: ChannelKind::Text,
                parent_id: None,
                topic: None,
                nsfw: false,
                slowmode_seconds: 0,
                overwrites: vec![],
            },
            reason: note.clone(),
        })
        .await
        .unwrap()
    else {
        panic!("expected a channel");
    };

    let outcome = async {
        // Nothing is asserted in here: a failed assert would skip the clean-up below.
        let found = live.api.channel_guild(channel.id).await?;

        let api: Arc<dyn DiscordApi> =
            Arc::new(TwilightApi::new(Arc::clone(&live.http), live.application));
        let granted = || {
            [Capability::SendMessages, Capability::ManageChannels]
                .into_iter()
                .collect()
        };
        let module = ModuleId::new("scope-test").unwrap();

        let elsewhere =
            ApiClient::new(Arc::clone(&api), granted(), module.clone()).scoped_to(Id::new(1));
        let refused = elsewhere.send_message(channel.id, "must not be sent").await;

        let here = ApiClient::new(api, granted(), module).scoped_to(live.guild);
        here.send_message(channel.id, "sent by a client bound to this guild")
            .await?;
        Ok::<_, DiscordError>((found, refused))
    }
    .await;

    live.api
        .execute(Action::DeleteChannel {
            channel_id: channel.id,
            reason: note,
        })
        .await
        .unwrap();

    let (found, refused) = outcome.unwrap();
    assert_eq!(
        found,
        Some(live.guild),
        "Discord says which guild the channel is in"
    );
    assert!(
        matches!(refused, Err(DiscordError::Forbidden(_))),
        "{refused:?}"
    );
}
