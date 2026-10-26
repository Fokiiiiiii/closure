//! The `automod` module: rules that check every message, the members and the roles, the cases,
//! the threads and the counters, and act on the first rule that hits.
//!
//! The rules are the server's to write (see `docs/automod.md`). This module is the frame they run
//! in: the events ([`context`]), the triggers and the spam windows ([`triggers`], [`spam`]), the
//! counters ([`counter`], [`commands`]), the actions and their record ([`actions`]), the pipeline
//! ([`engine`]), the configuration with its tolerant reading of each rule ([`config`]), and
//! `/automod status`. It is off until a server turns it on.

mod actions;
mod antiraid;
mod commands;
mod config;
mod context;
mod counter;
mod engine;
mod memory;
mod schema;
mod spam;
mod status;
mod triggers;

use std::sync::Arc;

use closure_sdk::punish;
use closure_sdk::punish::{Method, RestrictionRole, ensure_role};
use closure_sdk::{
    Capability, Command, CoreError, Event, EventContext, EventKind, Flow, Intents, Level, Locale,
    ModuleBuilder, ModuleError, PermissionSpec, SdkModule, SubcommandGroup, Subscription,
    parse_module_config,
};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

/// `/automod` is made by two calls (its status and its counters), which both give this text.
const AUTOMOD_DESCRIPTION: &str = "Automatic moderation: its rules and state";

/// The events the rules read: messages, members, roles, cases and threads.
const EVENTS: [EventKind; 10] = [
    EventKind::MessageCreated,
    EventKind::MessageUpdated,
    EventKind::MemberJoined,
    EventKind::MemberLeft,
    EventKind::RolesChanged,
    EventKind::CaseCreated,
    EventKind::ThreadCreated,
    EventKind::ThreadArchived,
    EventKind::ThreadUnarchived,
    EventKind::ThreadDeleted,
];

/// The events of the role method: a configuration change sets the timeout role up, and a new channel
/// gets its overwrites.
fn with_role_events(builder: ModuleBuilder) -> ModuleBuilder {
    builder
        .on(Subscription::new(EventKind::ConfigChanged), config_changed)
        .on(
            Subscription::new(EventKind::ChannelCreated),
            channel_created,
        )
}

/// Sets the timeout role up when the configuration changes, so that it exists before the first
/// punishment, as moderation does.
async fn config_changed(ctx: EventContext, _event: Event) -> Result<Flow, ModuleError> {
    let config: config::Config = parse_module_config(&ctx)?;
    if config.timeout_method == Method::Role {
        ensure_role(
            &ctx,
            RestrictionRole::Timeout,
            config::role_of(config.timeout_role.as_deref()),
        )
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    Ok(Flow::Continue)
}

/// A channel created later gets the overwrites of the timeout role, so it keeps the restriction.
async fn channel_created(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    if let Event::ChannelCreated(channel) = event {
        punish::channel_created(&ctx, channel.id).await?;
    }
    Ok(Flow::Continue)
}

/// The module, still open for a test to build on.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = schema::settings();
    // One state for the events and the commands: the counters a command changes are the ones the
    // rules read, and the bot's own role changes are remembered for the events.
    let state = Arc::new(engine::State::new());
    let on_event = {
        let state = Arc::clone(&state);
        move |ctx: EventContext, event: Event| {
            let state = Arc::clone(&state);
            async move { engine::on_event(&state, ctx, event).await }
        }
    };
    let mut builder = ModuleBuilder::new("automod", "Automod", env!("CARGO_PKG_VERSION"))?
        .description(
            "Automatic moderation: rules that check every message, and act on the first one that hits.",
        )
        .capability(Capability::Cases)
        .capability(Capability::SendMessages)
        .capability(Capability::ManageMessages)
        .capability(Capability::SendDirectMessages)
        .capability(Capability::KickMembers)
        .capability(Capability::BanMembers)
        .capability(Capability::TimeoutMembers)
        .capability(Capability::ManageRoles)
        .capability(Capability::ManageNicknames)
        .capability(Capability::ManageChannels)
        .capability(Capability::Storage)
        .capability(Capability::Schedule)
        .capability(Capability::ReadGuild)
        .capability(Capability::ReadMessageHistory)
        .capability(Capability::ManageGuild)
        // MESSAGE_CONTENT is privileged: it must be switched on for the bot in the Discord developer
        // portal, or the rules see no text at all. GUILD_MEMBERS is privileged too: the joins, the
        // leaves and the role changes need it.
        .intents(Intents::GUILD_MESSAGES | Intents::GUILD_MEMBERS | Intents::MESSAGE_CONTENT);
    for kind in EVENTS {
        builder = builder.on(Subscription::new(kind), on_event.clone());
    }
    builder = with_role_events(builder);
    let builder = builder
        .permission(PermissionSpec::new(
            "automod.view",
            Level::MODERATOR,
            "See the rules and the state of automod",
        )?)
        .permission(PermissionSpec::new(
            "automod.counters",
            Level::MODERATOR,
            "Look at and change the counters of automod",
        )?)
        .permission(PermissionSpec::new(
            "automod.antiraid.view",
            Level::MODERATOR,
            "See the antiraid level and its quarantine",
        )?)
        .permission(PermissionSpec::new(
            "automod.antiraid.set",
            Level::ADMIN,
            "Raise or lower the antiraid level",
        )?)
        .permission(PermissionSpec::new(
            "automod.antiraid.release",
            Level::MODERATOR,
            "Release members from the antiraid quarantine",
        )?)
        .config(schema, defaults)
        .group(
            "automod",
            AUTOMOD_DESCRIPTION,
            [
                Command::<()>::new("status", "Show whether dry run is on and which rules run")
                    .requires("automod.view")
                    .handler(status::status),
            ],
        )
        .groups(
            "automod",
            AUTOMOD_DESCRIPTION,
            [counters_group(&state), antiraid_group(&state)],
        )
        .autocomplete("automod antiraid set", "level", {
            let state = Arc::clone(&state);
            move |cx| {
                let state = Arc::clone(&state);
                async move { commands::level_choices(&state, &cx) }
            }
        })
        .job(actions::CLEANUP_JOB, actions::cleanup)
        .job(antiraid::EXPIRE_JOB, {
            let state = Arc::clone(&state);
            move |cx| {
                let state = Arc::clone(&state);
                async move { antiraid::expire(&state, cx).await }
            }
        });
    Ok(punish::jobs(builder)
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA))
}

/// `/automod counters`: the commands that look at and change the counters.
fn counters_group(state: &Arc<engine::State>) -> SubcommandGroup {
    SubcommandGroup::new("counters", "Look at and change the counters of automod")
        .subcommand(
            Command::<commands::View>::new("view", "Show the value of a counter")
                .requires("automod.counters")
                .handler({
                    let state = Arc::clone(state);
                    move |cx, args| commands::view(Arc::clone(&state), cx, args)
                }),
        )
        .subcommand(
            Command::<commands::Set>::new("set", "Set the value of a counter")
                .requires("automod.counters")
                .handler({
                    let state = Arc::clone(state);
                    move |cx, args| commands::set(Arc::clone(&state), cx, args)
                }),
        )
        .subcommand(
            Command::<commands::Add>::new("add", "Add to a counter")
                .requires("automod.counters")
                .handler({
                    let state = Arc::clone(state);
                    move |cx, args| commands::add(Arc::clone(&state), cx, args)
                }),
        )
        .subcommand(
            Command::<commands::Reset>::new("reset", "Put a counter back to its start")
                .requires("automod.counters")
                .handler({
                    let state = Arc::clone(state);
                    move |cx, args| commands::reset(Arc::clone(&state), cx, args)
                }),
        )
}

/// `/automod antiraid`: the antiraid level, and the quarantine of the members who joined at a level
/// that has one.
fn antiraid_group(state: &Arc<engine::State>) -> SubcommandGroup {
    SubcommandGroup::new(
        "antiraid",
        "Raise or lower the antiraid level, and release the quarantined members",
    )
    .subcommand(
        Command::<()>::new("view", "Show the antiraid level and the quarantine")
            .requires("automod.antiraid.view")
            .handler({
                let state = Arc::clone(state);
                move |cx, args| commands::antiraid_view(Arc::clone(&state), cx, args)
            }),
    )
    .subcommand(
        Command::<commands::AntiraidSet>::new("set", "Raise or lower the antiraid level")
            .requires("automod.antiraid.set")
            .handler({
                let state = Arc::clone(state);
                move |cx, args| commands::antiraid_set(Arc::clone(&state), cx, args)
            }),
    )
    .subcommand(
        Command::<()>::new("clear", "Put the antiraid level back to the calm one")
            .requires("automod.antiraid.set")
            .handler({
                let state = Arc::clone(state);
                move |cx, args| commands::antiraid_clear(Arc::clone(&state), cx, args)
            }),
    )
    .subcommand(
        Command::<commands::AntiraidRelease>::new("release", "Take a member out of the quarantine")
            .requires("automod.antiraid.release")
            .handler({
                let state = Arc::clone(state);
                move |cx, args| commands::antiraid_release(Arc::clone(&state), cx, args)
            }),
    )
    .subcommand(
        Command::<()>::new("release-all", "Take every member out of the quarantine")
            .requires("automod.antiraid.release")
            .handler({
                let state = Arc::clone(state);
                move |cx, args| commands::antiraid_release_all(Arc::clone(&state), cx, args)
            }),
    )
}

/// The module as the host loads it.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}
