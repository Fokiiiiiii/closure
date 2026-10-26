//! The `automod` module: rules that check every message, and act on the first one that hits.
//!
//! The rules are the server's to write (see `docs/automod.md`). This module is the frame they run
//! in: the triggers ([`triggers`], and the spam windows of [`spam`]), the actions and their record
//! ([`actions`]), the pipeline ([`engine`]), the configuration with its tolerant reading of each
//! rule ([`config`]), and the `/automod status` command. It is off until a server turns it on.

mod actions;
mod config;
mod engine;
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

/// The module, still open for a test to build on.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = schema::settings();
    let state = Arc::new(engine::State::new());
    let on_message = move |ctx: EventContext, event: Event| {
        let state = Arc::clone(&state);
        async move { engine::on_message(&state, ctx, event).await }
    };
    let builder = ModuleBuilder::new("automod", "Automod", env!("CARGO_PKG_VERSION"))?
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
        .capability(Capability::Storage)
        .capability(Capability::Schedule)
        .capability(Capability::ReadGuild)
        .capability(Capability::ReadMessageHistory)
        // MESSAGE_CONTENT is privileged: it must be switched on for the bot in the Discord developer
        // portal, or the rules see no text at all.
        .intents(Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT)
        .on(
            Subscription::new(EventKind::MessageCreated),
            on_message.clone(),
        )
        .on(Subscription::new(EventKind::MessageUpdated), on_message)
        .permission(PermissionSpec::new(
            "automod.view",
            Level::MODERATOR,
            "See the rules and the state of automod",
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
        .job(actions::CLEANUP_JOB, actions::cleanup);
    Ok(punish::jobs(builder)
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA))
}

/// The module as the host loads it.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}
