//! Following a member into voice: a moderator who follows a member gets a direct message each
//! time that member joins a voice channel or moves to another.
//!
//! Two sets in the module's storage keep it: `follow:<member>` holds the moderators who follow
//! the member, and `following:<moderator>` holds the members a moderator follows (for the list).

use closure_core::event::{Event, EventKind, UserId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, Capability, Command, CommandArgs, CommandCx, CoreError, Intents, Level, Locale,
    ModuleBuilder, ModuleError, PermissionSpec, SdkModule, Subscription, UserError,
};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const MAX_FOLLOWERS: usize = 25;
const MAX_LISTED: usize = 50;

#[derive(CommandArgs)]
struct Target {
    /// The member.
    user: UserId,
}

fn followers_of(user: u64) -> String {
    format!("follow:{user}")
}

fn following_of(moderator: u64) -> String {
    format!("following:{moderator}")
}

fn failed(error: &closure_core::storage::StorageError) -> UserError {
    tracing::warn!(%error, "follow storage failed");
    UserError::message("follow-failed")
}

async fn add(cx: CommandCx, args: Target) -> Result<(), UserError> {
    let (moderator, user) = (cx.user().id.get(), args.user.get());
    let store = cx.store();
    store
        .set_add(&followers_of(user), moderator)
        .await
        .map_err(|e| failed(&e))?;
    store
        .set_add(&following_of(moderator), user)
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t(
        "follow-added",
        &Args::new().with("user", format!("<@{user}>")),
    )
    .await
}

async fn remove(cx: CommandCx, args: Target) -> Result<(), UserError> {
    let (moderator, user) = (cx.user().id.get(), args.user.get());
    let store = cx.store();
    store
        .set_remove(&followers_of(user), moderator)
        .await
        .map_err(|e| failed(&e))?;
    store
        .set_remove(&following_of(moderator), user)
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t(
        "follow-removed",
        &Args::new().with("user", format!("<@{user}>")),
    )
    .await
}

async fn list(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let followed = cx
        .store()
        .set_sample(&following_of(cx.user().id.get()), MAX_LISTED)
        .await
        .map_err(|e| failed(&e))?;
    if followed.is_empty() {
        return cx.reply_private_t("follow-none", &Args::new()).await;
    }
    let users: Vec<String> = followed.iter().map(|id| format!("<@{id}>")).collect();
    cx.reply_private_t("follow-list", &Args::new().with("users", users.join(", ")))
        .await
}

async fn on_voice(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::VoiceStateChanged {
        user,
        before,
        after: Some(channel),
        ..
    } = event
    else {
        return Ok(Flow::Continue);
    };
    if before == Some(channel) || user.bot {
        return Ok(Flow::Continue);
    }
    let moderators = ctx
        .store()
        .set_sample(&followers_of(user.id.get()), MAX_FOLLOWERS)
        .await
        .map_err(|e| ModuleError::other(e.to_string()))?;
    for moderator in moderators.into_iter().filter_map(UserId::new_checked) {
        let text = ctx.t_in(
            ctx.locale,
            "follow-notice",
            &Args::new()
                .with("user", format!("<@{}>", user.id))
                .with("channel", format!("<#{channel}>")),
        );
        if let Err(error) = ctx
            .api
            .send_direct_message(moderator, text, Vec::new())
            .await
        {
            tracing::warn!(%error, "could not tell a moderator about a followed member");
        }
    }
    Ok(Flow::Continue)
}

/// The module before it is built, so that tests can run it on the SDK's pretend Discord.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("follow", "Follow", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::Storage)
            .capability(Capability::SendDirectMessages)
            .intents(Intents::GUILD_VOICE_STATES)
            .permission(PermissionSpec::new(
                "follow.use",
                Level::MODERATOR,
                "permission-follow-use",
            )?)
            .on(Subscription::new(EventKind::VoiceStateChanged), on_voice)
            .group(
                "follow",
                [
                    Command::<Target>::new("add")
                        .requires("follow.use")
                        .handler(add),
                    Command::<Target>::new("remove")
                        .requires("follow.use")
                        .handler(remove),
                    Command::<()>::new("list")
                        .requires("follow.use")
                        .handler(list),
                ],
            )
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
