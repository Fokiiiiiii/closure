//! The `automod` module: rules that check every message, the members and the roles, the cases,
//! the threads and the counters, and act on the first rule that hits.
//!
//! The rules are the server's to write (see `docs/automod.md`). This module is the frame they run
//! in: the events ([`context`]), the triggers and the spam windows ([`triggers`], [`spam`]), the
//! counters ([`counter`], [`commands`]), the actions and their record ([`actions`]), the pipeline
//! ([`engine`]), the configuration with its tolerant reading of each rule ([`config`]), and
//! `/automod status`. It is off until a server turns it on.

mod actions;
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
use closure_sdk::{
    Capability, Command, CoreError, Event, EventContext, EventKind, Intents, Level, Locale,
    ModuleBuilder, PermissionSpec, SdkModule, Subscription,
};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

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
        // MESSAGE_CONTENT is privileged: it must be switched on for the bot in the Discord developer
        // portal, or the rules see no text at all. GUILD_MEMBERS is privileged too: the joins, the
        // leaves and the role changes need it.
        .intents(Intents::GUILD_MESSAGES | Intents::GUILD_MEMBERS | Intents::MESSAGE_CONTENT);
    for kind in EVENTS {
        builder = builder.on(Subscription::new(kind), on_event.clone());
    }
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
        .config(schema, defaults)
        .group(
            "automod",
            "Automatic moderation: its rules and state",
            [
                Command::<()>::new("status", "Show whether dry run is on and which rules run")
                    .requires("automod.view")
                    .handler(status::status),
            ],
        )
        .group(
            "automod-counters",
            "Look at and change the counters of automod",
            [
                Command::<commands::View>::new("view", "Show the value of a counter")
                    .requires("automod.counters")
                    .handler({
                        let state = Arc::clone(&state);
                        move |cx, args| commands::view(Arc::clone(&state), cx, args)
                    }),
                Command::<commands::Set>::new("set", "Set the value of a counter")
                    .requires("automod.counters")
                    .handler({
                        let state = Arc::clone(&state);
                        move |cx, args| commands::set(Arc::clone(&state), cx, args)
                    }),
                Command::<commands::Add>::new("add", "Add to a counter")
                    .requires("automod.counters")
                    .handler({
                        let state = Arc::clone(&state);
                        move |cx, args| commands::add(Arc::clone(&state), cx, args)
                    }),
                Command::<commands::Reset>::new("reset", "Put a counter back to its start")
                    .requires("automod.counters")
                    .handler({
                        let state = Arc::clone(&state);
                        move |cx, args| commands::reset(Arc::clone(&state), cx, args)
                    }),
            ],
        )
        .job(actions::CLEANUP_JOB, actions::cleanup);
    Ok(punish::jobs(builder)
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA))
}

/// The module as the host loads it.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}
