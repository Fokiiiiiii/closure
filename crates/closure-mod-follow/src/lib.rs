//! Following a member into voice: a moderator who follows a member gets a direct message each
//! time that member joins a voice channel or moves to another.
//!
//! Two sets in the module's storage keep it: `follow:<member>` holds the moderators who follow
//! the member and is the source of truth, and `following:<moderator>` holds the members a
//! moderator follows, as an index for the list.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use closure_core::action::{Action, Outcome};
use closure_core::event::{Event, EventKind, UserId};
use closure_core::module::EventContext;
use closure_core::permission::{Actor, PermissionKey};
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, Capability, Command, CommandArgs, CommandCx, CoreError, Intents, Level, Locale,
    ModuleBuilder, ModuleError, PermissionSpec, SdkModule, Subscription, UserError,
};
use twilight_model::guild::{Permissions, Role};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const MAX_FOLLOWERS: usize = 25;
const MAX_FOLLOWING: usize = 50;
const NOTICE_WINDOW: Duration = Duration::from_secs(10);
const MAX_NOTICE_ENTRIES: usize = 1024;

/// The last time (unix seconds) each member's moderators were told, by guild and member.
type NoticeTable = Arc<Mutex<HashMap<(u64, u64), i64>>>;

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

fn is_full(count: u64, limit: usize) -> bool {
    usize::try_from(count).is_ok_and(|count| count >= limit)
}

async fn add(cx: CommandCx, args: Target) -> Result<(), UserError> {
    let (moderator, user) = (cx.user().id.get(), args.user.get());
    let store = cx.store();
    let already = store
        .set_has(&followers_of(user), moderator)
        .await
        .map_err(|e| failed(&e))?;
    if !already {
        let followers = store
            .set_count(&followers_of(user))
            .await
            .map_err(|e| failed(&e))?;
        if is_full(followers, MAX_FOLLOWERS) {
            return Err(UserError::message("follow-full-member"));
        }
        let following = store
            .set_count(&following_of(moderator))
            .await
            .map_err(|e| failed(&e))?;
        if is_full(following, MAX_FOLLOWING) {
            return Err(UserError::message("follow-full-moderator"));
        }
    }
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
        .set_remove(&following_of(moderator), user)
        .await
        .map_err(|e| failed(&e))?;
    store
        .set_remove(&followers_of(user), moderator)
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t(
        "follow-removed",
        &Args::new().with("user", format!("<@{user}>")),
    )
    .await
}

async fn list(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let moderator = cx.user().id.get();
    let store = cx.store();
    let mut followed = Vec::new();
    for user in store
        .set_sample(&following_of(moderator), MAX_FOLLOWING)
        .await
        .map_err(|e| failed(&e))?
    {
        if store
            .set_has(&followers_of(user), moderator)
            .await
            .map_err(|e| failed(&e))?
        {
            followed.push(user);
        } else {
            store
                .set_remove(&following_of(moderator), user)
                .await
                .map_err(|e| failed(&e))?;
        }
    }
    if followed.is_empty() {
        return cx.reply_private_t("follow-none", &Args::new()).await;
    }
    let users: Vec<String> = followed.iter().map(|id| format!("<@{id}>")).collect();
    cx.reply_private_t("follow-list", &Args::new().with("users", users.join(", ")))
        .await
}

/// Whether a notice about `member` was not sent within the last window in this guild; records the
/// time if so.
fn claim_notice(notices: &NoticeTable, guild: u64, member: u64, now: i64) -> bool {
    let window = i64::try_from(NOTICE_WINDOW.as_secs()).unwrap_or(i64::MAX);
    let key = (guild, member);
    let mut last = notices.lock().unwrap_or_else(PoisonError::into_inner);
    if last.get(&key).is_some_and(|at| now - *at < window) {
        return false;
    }
    if !last.contains_key(&key) && last.len() >= MAX_NOTICE_ENTRIES {
        last.retain(|_, at| now - *at < window);
        if last.len() >= MAX_NOTICE_ENTRIES {
            let oldest = last.iter().min_by_key(|(_, at)| **at).map(|(id, _)| *id);
            if let Some(oldest) = oldest {
                last.remove(&oldest);
            }
        }
    }
    last.insert(key, now);
    true
}

/// Whether the moderator still may use `follow.use`, decided as the command decides it. `None`
/// when the member cannot be read: nothing is taken away on a guess.
async fn holds_follow_permission(
    ctx: &EventContext,
    moderator: UserId,
    owner: Option<UserId>,
    roles: &[Role],
) -> Option<bool> {
    let member = ctx.api.get_member(ctx.guild_id, moderator).await.ok()?;
    // Discord's Administrator permission comes from the member's roles and from @everyone.
    let everyone = roles
        .iter()
        .find(|role| role.id.get() == ctx.guild_id.get());
    let held = roles
        .iter()
        .filter(|role| member.roles.contains(&role.id))
        .chain(everyone)
        .fold(Permissions::empty(), |all, role| all | role.permissions);
    let actor = Actor {
        user: moderator,
        roles: member.roles,
        administrator: held.contains(Permissions::ADMINISTRATOR),
        owner: owner == Some(moderator),
    };
    let key = PermissionKey::new("follow.use").ok()?;
    Some(ctx.permissions().check(&actor, &key).allowed)
}

/// Ends a follow of a moderator who lost the permission; the index goes first, as in `remove`.
async fn revoke(ctx: &EventContext, member: u64, moderator: u64) {
    let store = ctx.store();
    if let Err(error) = store.set_remove(&following_of(moderator), member).await {
        tracing::warn!(%error, "could not drop a follow of a moderator without the permission");
        return;
    }
    if let Err(error) = store.set_remove(&followers_of(member), moderator).await {
        tracing::warn!(%error, "could not drop a follow of a moderator without the permission");
    }
}

async fn on_voice(
    ctx: EventContext,
    event: Event,
    notices: NoticeTable,
) -> Result<Flow, ModuleError> {
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
    let member = user.id.get();
    let moderators = ctx
        .store()
        .set_sample(&followers_of(member), MAX_FOLLOWERS)
        .await
        .map_err(|e| ModuleError::other(e.to_string()))?;
    if moderators.is_empty() || !claim_notice(&notices, ctx.guild_id.get(), member, ctx.now()) {
        return Ok(Flow::Continue);
    }
    let owner = match ctx.api.guild_owner().await {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(%error, "could not read the guild owner; follow notices skipped");
            return Ok(Flow::Continue);
        }
    };
    // One read of the roles serves every follower of this event.
    let Ok(Outcome::Roles(roles)) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
    else {
        tracing::warn!("could not read the roles; follow notices skipped");
        return Ok(Flow::Continue);
    };
    for moderator in moderators.into_iter().filter_map(UserId::new_checked) {
        match holds_follow_permission(&ctx, moderator, owner, &roles).await {
            Some(true) => {}
            Some(false) => {
                revoke(&ctx, member, moderator.get()).await;
                continue;
            }
            None => {
                tracing::warn!("could not check a follower's permission; not told");
                continue;
            }
        }
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
    let notices: NoticeTable = Arc::new(Mutex::new(HashMap::new()));
    Ok(
        ModuleBuilder::new("follow", "Follow", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::Storage)
            .capability(Capability::SendDirectMessages)
            // The member's roles, read at delivery to check each follower still holds the permission.
            .capability(Capability::ReadGuild)
            .intents(Intents::GUILD_VOICE_STATES)
            .permission(PermissionSpec::new(
                "follow.use",
                Level::MODERATOR,
                "permission-follow-use",
            )?)
            .on(
                Subscription::new(EventKind::VoiceStateChanged),
                move |ctx, event| on_voice(ctx, event, Arc::clone(&notices)),
            )
            .group(
                "follow",
                [
                    Command::<Target>::new("add")
                        .requires("follow.use")
                        .handler(add),
                    // Without the permission a moderator must still be able to stop following.
                    Command::<Target>::new("remove").handler(remove),
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
