//! Roles that members take themselves, and roles that moderators hand out.
//!
//! A role is only ever given by the module when the server listed it: members take what is in
//! `self_roles`, with a command, a button or a reaction. Moderators give any role that sits below
//! their own highest one.

use closure_core::action::{Action, Outcome, Reason, RolePatch};
use closure_core::error::DiscordError;
use closure_core::event::{Event, EventKind, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::permission::{Actor, PermissionKey};
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, Command, CommandArgs, CommandCx, ComponentCx, CoreError, Intents, Level, Locale,
    ModuleBuilder, ModuleError, PermissionSpec, SdkModule, Subscription, UserError,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::guild::Permissions;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// The route of the buttons that give or take a role.
const TOGGLE: &str = "toggle";
/// The job that makes a role unpingable again.
const UNPING: &str = "unping";
/// How long a role can be pinged after `/pingrole`.
const PING_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// The score that holds, for a role made pingable, when it is to be quiet again (unix seconds).
const PING_UNTIL: &str = "ping-until";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_field_names)]
struct Config {
    self_roles: Vec<String>,
    reaction_roles: Vec<ReactionRole>,
    /// Roles that can be pinged for a minute with `/pingrole`.
    pingable_roles: Vec<String>,
}

#[derive(Debug, serde::Serialize, Deserialize)]
struct Unping {
    role: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactionRole {
    channel: String,
    message: String,
    emoji: String,
    role: String,
}

impl Config {
    fn allows(&self, role: RoleId) -> bool {
        self.self_roles
            .iter()
            .any(|r| r.parse::<u64>().ok() == Some(role.get()))
    }
}

#[derive(CommandArgs)]
struct OneRole {
    /// The role.
    role: RoleId,
}

#[derive(CommandArgs)]
struct MemberRole {
    /// The member.
    user: UserId,
    /// The role.
    role: RoleId,
}

fn problem(error: &DiscordError) -> UserError {
    match error {
        DiscordError::Forbidden(_) | DiscordError::HierarchyViolation(_) => {
            UserError::message("roles-cannot")
        }
        DiscordError::NotFound => UserError::message("roles-not-found"),
        _ => UserError::message("roles-unavailable"),
    }
}

fn mention(role: RoleId) -> String {
    format!("<@&{role}>")
}

/// Whether `role` is the role of a punishment. A role that cannot be looked up counts as one:
/// nothing is handed out on a guess.
async fn restricted(ctx: &EventContext, role: RoleId) -> bool {
    closure_sdk::punish::is_restriction_role(ctx, role)
        .await
        .unwrap_or(true)
}

/// Gives or takes a role the server listed for its members.
async fn self_service(cx: &CommandCx, role: RoleId, give: bool) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    if !config.allows(role) {
        return Err(UserError::message("roles-not-listed"));
    }
    if restricted(cx.ctx(), role).await {
        return Err(UserError::message("roles-restricted"));
    }
    let reason = Reason::new(cx.t("roles-reason-self", &Args::new())).map_err(|e| problem(&e))?;
    let (guild, user) = (cx.guild_id(), cx.user().id);
    let result = if give {
        cx.api().add_role(guild, user, role, reason).await
    } else {
        cx.api().remove_role(guild, user, role, reason).await
    };
    result.map_err(|e| problem(&e))?;
    cx.reply_private_t(
        if give { "roles-joined" } else { "roles-left" },
        &Args::new().with("role", mention(role)),
    )
    .await
}

async fn join(cx: CommandCx, args: OneRole) -> Result<(), UserError> {
    self_service(&cx, args.role, true).await
}

async fn leave(cx: CommandCx, args: OneRole) -> Result<(), UserError> {
    self_service(&cx, args.role, false).await
}

async fn list(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    if config.self_roles.is_empty() {
        return cx.reply_private_t("roles-list-empty", &Args::new()).await;
    }
    let roles: Vec<String> = config
        .self_roles
        .iter()
        .filter_map(|r| RoleId::new_checked(r.parse::<u64>().ok()?))
        .map(mention)
        .collect();
    cx.reply_private(roles.join("\n")).await
}

/// Whether the invoker may hand out `role`: it must sit below their highest role. The owner may
/// hand out any.
async fn may_hand_out(cx: &CommandCx, role: RoleId) -> Result<(), UserError> {
    let actor = cx.actor().await;
    if actor.owner {
        return Ok(());
    }
    let Ok(Outcome::Roles(roles)) = cx
        .api()
        .execute(Action::ListRoles {
            guild_id: cx.guild_id(),
        })
        .await
    else {
        return Err(UserError::message("roles-unavailable"));
    };
    let position = |id: RoleId| roles.iter().find(|r| r.id == id).map(|r| r.position);
    let target = position(role).ok_or_else(|| UserError::message("roles-not-found"))?;
    let own = actor
        .roles
        .iter()
        .filter_map(|r| position(*r))
        .max()
        .unwrap_or(0);
    if target >= own {
        return Err(UserError::message("roles-too-high"));
    }
    // Position alone is not enough: a role below theirs may carry powers they do not have, and
    // handing it out would be a way to take them.
    if !actor.administrator {
        let held = roles
            .iter()
            .filter(|r| actor.roles.contains(&r.id) || r.id.get() == cx.guild_id().get())
            .fold(Permissions::empty(), |all, r| all | r.permissions);
        let granted = roles
            .iter()
            .find(|r| r.id == role)
            .map_or(Permissions::empty(), |r| r.permissions);
        if !held.contains(granted) {
            return Err(UserError::message("roles-too-powerful"));
        }
    }
    Ok(())
}

async fn hand_out(cx: &CommandCx, args: MemberRole, give: bool) -> Result<(), UserError> {
    may_hand_out(cx, args.role).await?;
    // A punishment's role is its own to give and take off, not a moderator's.
    if restricted(cx.ctx(), args.role).await {
        return Err(UserError::message("roles-restricted"));
    }
    // The member is held to the same hierarchy as for a punishment, except for oneself.
    if args.user != cx.user().id {
        cx.check_target(args.user).await?;
    }
    let reason = Reason::new(cx.t(
        "roles-reason-moderator",
        &Args::new().with("moderator", cx.user().name.clone()),
    ))
    .map_err(|e| problem(&e))?;
    let guild = cx.guild_id();
    let result = if give {
        cx.api().add_role(guild, args.user, args.role, reason).await
    } else {
        cx.api()
            .remove_role(guild, args.user, args.role, reason)
            .await
    };
    result.map_err(|e| problem(&e))?;
    cx.reply_t(
        if give { "roles-given" } else { "roles-taken" },
        &Args::new()
            .with("role", mention(args.role))
            .with("user", format!("<@{}>", args.user)),
    )
    .await
}

async fn give(cx: CommandCx, args: MemberRole) -> Result<(), UserError> {
    hand_out(&cx, args, true).await
}

async fn take(cx: CommandCx, args: MemberRole) -> Result<(), UserError> {
    hand_out(&cx, args, false).await
}

/// A button of a message the server posted: it gives the role, or takes it when the member has it.
async fn toggle(cx: ComponentCx) -> Result<(), UserError> {
    // A button is another way in to what `/role` gives: the same permission is asked.
    cx.require("roles.self").await?;
    let Some(role) = cx
        .data()
        .parse::<u64>()
        .ok()
        .filter(|r| *r > 0)
        .map(RoleId::new)
    else {
        return Err(UserError::message("roles-not-listed"));
    };
    let config: Config = cx.config()?;
    if !config.allows(role) {
        return Err(UserError::message("roles-not-listed"));
    }
    if restricted(cx.ctx(), role).await {
        return Err(UserError::message("roles-restricted"));
    }
    let has = cx.click().roles.contains(&role);
    let reason = Reason::new(cx.t("roles-reason-self", &Args::new())).map_err(|e| problem(&e))?;
    let (guild, user) = (cx.guild_id(), cx.user().id);
    let result = if has {
        cx.api().remove_role(guild, user, role, reason).await
    } else {
        cx.api().add_role(guild, user, role, reason).await
    };
    result.map_err(|e| problem(&e))?;
    cx.reply_private_t(
        if has { "roles-left" } else { "roles-joined" },
        &Args::new().with("role", mention(role)),
    )
    .await
}

/// Lets a listed role be pinged for a minute: it is mentionable until the job takes that back.
async fn ping_role(cx: CommandCx, args: OneRole) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    if !config
        .pingable_roles
        .iter()
        .any(|r| r.parse::<u64>().ok() == Some(args.role.get()))
    {
        return Err(UserError::message("roles-not-pingable"));
    }
    // The job that takes the permission back is kept first: a role that can be pinged must always
    // have one waiting to make it quiet again.
    let schedule = |e: closure_core::schedule::ScheduleError| {
        UserError::from(ModuleError::other(e.to_string()))
    };
    cx.ctx()
        .store()
        .score_put(
            PING_UNTIL,
            args.role.get(),
            cx.now()
                .saturating_add(i64::try_from(PING_WINDOW.as_secs()).unwrap_or(0)),
        )
        .await
        .map_err(|e| UserError::from(ModuleError::other(e.to_string())))?;
    cx.scheduler()
        .job(
            UNPING,
            &Unping {
                role: args.role.get(),
            },
        )
        .map_err(schedule)?
        .key(&format!("unping:{}", args.role))
        .after(PING_WINDOW)
        .await
        .map_err(schedule)?;
    let reason = Reason::new(cx.t("roles-reason-ping", &Args::new())).map_err(|e| problem(&e))?;
    cx.api()
        .execute(Action::EditRole {
            guild_id: cx.guild_id(),
            role_id: args.role,
            patch: RolePatch {
                mentionable: Some(true),
                ..RolePatch::default()
            },
            reason,
        })
        .await
        .map_err(|e| problem(&e))?;
    cx.reply_private_t(
        "roles-pingable",
        &Args::new().with("role", mention(args.role)),
    )
    .await
}

async fn unping(cx: closure_sdk::JobCx) -> Result<(), ModuleError> {
    let job: Unping = cx.payload()?;
    let role = RoleId::new_checked(job.role)
        .ok_or_else(|| ModuleError::other("an unping job names no role"))?;
    // A later ping of the same role moved the end: that one's job makes it quiet, not this one,
    // which may have been taken before the ping was made again.
    let until = cx
        .ctx()
        .store()
        .score(PING_UNTIL, role.get())
        .await
        .map_err(|e| ModuleError::other(e.to_string()))?;
    if until > cx.ctx().now() {
        return Ok(());
    }
    let reason = Reason::new(cx.t("roles-reason-ping-end", &Args::new()))?;
    let result = cx
        .api()
        .execute(Action::EditRole {
            guild_id: cx.guild_id(),
            role_id: role,
            patch: RolePatch {
                mentionable: Some(false),
                ..RolePatch::default()
            },
            reason,
        })
        .await;
    match result {
        Ok(_) | Err(DiscordError::NotFound) => Ok(()),
        Err(error) if error.is_retryable() => Err(error.into()),
        Err(error) => {
            tracing::warn!(?error, "could not make a role unpingable again");
            Ok(())
        }
    }
}

fn emoji_text(emoji: &closure_core::action::Emoji) -> Vec<String> {
    match emoji {
        closure_core::action::Emoji::Unicode { value } => vec![value.clone()],
        closure_core::action::Emoji::Custom { id, name } => {
            let mut keys = vec![id.to_string()];
            if let Some(name) = name {
                keys.push(format!("{name}:{id}"));
                keys.push(format!("<:{name}:{id}>"));
                keys.push(format!("<a:{name}:{id}>"));
            }
            keys
        }
    }
}

/// Whether `user` holds the permission to pick roles for themselves (`roles.self`).
async fn may_pick(ctx: &EventContext, user: UserId) -> bool {
    let Ok(member) = ctx.api.get_member(ctx.guild_id, user).await else {
        return false;
    };
    let owner = ctx.api.guild_owner().await.ok().flatten() == Some(user);
    let actor = Actor {
        user,
        roles: member.roles,
        administrator: false,
        owner,
    };
    PermissionKey::new("roles.self").is_ok_and(|key| ctx.permissions().check(&actor, &key).allowed)
}

/// A reaction on a message the server made a reaction role of: gives the role, or takes it back
/// when the reaction is removed.
async fn reaction(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let (info, give) = match event {
        Event::ReactionAdded(info) => (info, true),
        Event::ReactionRemoved(info) => (info, false),
        _ => return Ok(Flow::Continue),
    };
    if ctx.bot() == Some(info.user_id) {
        return Ok(Flow::Continue);
    }
    let config: Config = ctx.parse_config()?;
    let keys = emoji_text(&info.emoji);
    let entry = config.reaction_roles.iter().find(|rule| {
        rule.channel.parse::<u64>().ok() == Some(info.channel_id.get())
            && rule.message.parse::<u64>().ok() == Some(info.message_id.get())
            && keys.iter().any(|k| *k == rule.emoji.trim())
    });
    let Some(role) = entry
        .and_then(|rule| rule.role.parse::<u64>().ok())
        .map(RoleId::new)
    else {
        return Ok(Flow::Continue);
    };
    if !config.allows(role) || restricted(&ctx, role).await {
        return Ok(Flow::Continue);
    }
    // The same permission as the command and the button: a member it is denied to gets nothing
    // from a reaction either. Taking a role back needs no permission.
    if give && !may_pick(&ctx, info.user_id).await {
        return Ok(Flow::Continue);
    }
    let reason = Reason::new(ctx.t_in(ctx.locale, "roles-reason-reaction", &Args::new()))?;
    let result = if give {
        ctx.api
            .add_role(ctx.guild_id, info.user_id, role, reason)
            .await
    } else {
        ctx.api
            .remove_role(ctx.guild_id, info.user_id, role, reason)
            .await
    };
    if let Err(error) = result {
        tracing::debug!(?error, guild = %ctx.guild_id, "could not give a reaction role");
    }
    Ok(Flow::Continue)
}

fn snowflake() -> serde_json::Value {
    json!({"type": "string", "pattern": "^[0-9]{1,20}$"})
}

#[allow(clippy::too_many_lines)]
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "self_roles": {
                "type": "array",
                "maxItems": 25,
                "items": snowflake(),
                "description": "config-roles-self_roles",
                "x-ui": { "level": "basic", "widget": "role" }
            },
            "pingable_roles": {
                "type": "array",
                "maxItems": 25,
                "items": snowflake(),
                "description": "config-roles-pingable_roles",
                "x-ui": { "level": "basic", "widget": "role" }
            },
            "reaction_roles": {
                "type": "array",
                "maxItems": 50,
                "description": "config-roles-reaction_roles",
                "x-ui": { "level": "basic" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["channel", "message", "emoji", "role"],
                    "properties": {
                        "channel": snowflake(),
                        "message": snowflake(),
                        "emoji": { "type": "string", "minLength": 1, "maxLength": 64 },
                        "role": snowflake()
                    }
                }
            }
        }
    });
    Ok(
        ModuleBuilder::new("roles", "Roles", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::ManageRoles)
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::ReadGuild)
            .capability(closure_sdk::Capability::RespondToInteractions)
            .intents(Intents::GUILD_MESSAGE_REACTIONS)
            .permission(PermissionSpec::new(
                "roles.self",
                Level::GENERAL,
                "permission-roles-self",
            )?)
            .permission(PermissionSpec::new(
                "roles.manage",
                Level::MODERATOR,
                "permission-roles-manage",
            )?)
            .permission(PermissionSpec::new(
                "roles.ping",
                Level::TRUSTED,
                "permission-roles-ping",
            )?)
            .config(
                schema,
                json!({"self_roles": [], "reaction_roles": [], "pingable_roles": []}),
            )
            .group(
                "role",
                [
                    Command::<OneRole>::new("join")
                        .requires("roles.self")
                        .handler(join),
                    Command::<OneRole>::new("leave")
                        .requires("roles.self")
                        .handler(leave),
                    Command::<()>::new("list")
                        .requires("roles.self")
                        .handler(list),
                    Command::<MemberRole>::new("give")
                        .requires("roles.manage")
                        .handler(give),
                    Command::<MemberRole>::new("take")
                        .requires("roles.manage")
                        .handler(take),
                ],
            )
            .command(
                Command::<OneRole>::new("pingrole")
                    .requires("roles.ping")
                    .handler(ping_role),
            )
            .job(UNPING, unping)
            .component(TOGGLE, toggle)
            .on(Subscription::new(EventKind::ReactionAdded), |ctx, event| {
                reaction(ctx, event)
            })
            .on(Subscription::new(EventKind::ReactionRemoved), reaction)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
