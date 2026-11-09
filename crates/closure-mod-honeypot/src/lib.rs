//! A honeypot: a channel nobody has a reason to write in except a bot or a raider. Whoever posts in
//! one is punished at once (banned, kicked or timed out, as the server chose) and the message is
//! deleted.
//!
//! The channels are kept in the module's storage and set with the `/honeypot` commands, which can
//! also make a channel with a warning in it. Moderators, the owner, the bot and the members the
//! server listed are never punished: a mistaken post cannot cost the server its staff.

use std::time::Duration;

use closure_core::action::{Action, ChannelKind, NewChannel, Outcome, Reason};
use closure_core::event::{Event, EventKind, MessageInfo, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::permission::Actor;
use closure_core::subscription::Flow;
use closure_sdk::duration::parse as parse_duration;
use closure_sdk::punish::{self, Moderator, Punisher, Settings};
use closure_sdk::{
    Args, Capability, ChannelId, Command, CommandArgs, CommandCx, CoreError, Intents, Level,
    Locale, ModuleBuilder, ModuleError, PermissionSpec, SdkModule, Subscription, UserError,
    parse_module_config,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::guild::Permissions;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const CHANNELS: &str = "honeypots";
/// The most channels listed in one reply.
const MAX_LISTED: usize = 50;
/// The most hours of a banned member's messages that are deleted.
const MAX_DELETE_HOURS: u32 = 168;

/// The powers that make a member one of the staff, who are never punished.
const STAFF: Permissions = Permissions::ADMINISTRATOR
    .union(Permissions::MANAGE_GUILD)
    .union(Permissions::MANAGE_ROLES)
    .union(Permissions::MANAGE_CHANNELS)
    .union(Permissions::BAN_MEMBERS)
    .union(Permissions::KICK_MEMBERS)
    .union(Permissions::MODERATE_MEMBERS)
    .union(Permissions::MANAGE_MESSAGES);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    #[default]
    Ban,
    Kick,
    Timeout,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    action: Method,
    /// How much of a banned member's messages is deleted, in hours.
    delete_hours: u32,
    /// How long a timeout lasts, such as 1d.
    timeout: String,
    exempt_users: Vec<String>,
    exempt_roles: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            action: Method::Ban,
            delete_hours: 24,
            timeout: "1d".to_owned(),
            exempt_users: Vec::new(),
            exempt_roles: Vec::new(),
        }
    }
}

impl Config {
    fn delete_seconds(&self) -> u32 {
        self.delete_hours.min(MAX_DELETE_HOURS) * 3_600
    }

    fn timeout(&self) -> Duration {
        parse_duration(&self.timeout)
            .unwrap_or(Duration::from_hours(24))
            .clamp(Duration::from_secs(60), Duration::from_hours(28 * 24))
    }
}

fn ids<T>(list: &[String], make: impl Fn(u64) -> Option<T>) -> Vec<T> {
    list.iter()
        .filter_map(|text| text.parse().ok())
        .filter_map(make)
        .collect()
}

fn failed(error: &closure_core::storage::StorageError) -> UserError {
    tracing::warn!(%error, "honeypot storage failed");
    UserError::message("honeypot-failed")
}

#[derive(CommandArgs)]
struct Pick {
    /// The channel.
    channel: ChannelId,
}

#[derive(CommandArgs)]
struct Create {
    /// The name of the new channel; left out, a name of the language of the server
    #[arg(max_length = 100)]
    name: Option<String>,
}

async fn add(cx: CommandCx, args: Pick) -> Result<(), UserError> {
    cx.store()
        .set_add(CHANNELS, args.channel.get())
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t(
        "honeypot-added",
        &Args::new().with("channel", format!("<#{}>", args.channel)),
    )
    .await
}

async fn remove(cx: CommandCx, args: Pick) -> Result<(), UserError> {
    cx.store()
        .set_remove(CHANNELS, args.channel.get())
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t(
        "honeypot-removed",
        &Args::new().with("channel", format!("<#{}>", args.channel)),
    )
    .await
}

async fn list(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let channels = cx
        .store()
        .set_sample(CHANNELS, MAX_LISTED)
        .await
        .map_err(|e| failed(&e))?;
    if channels.is_empty() {
        return cx.reply_private_t("honeypot-none", &Args::new()).await;
    }
    let text: Vec<String> = channels.iter().map(|id| format!("<#{id}>")).collect();
    cx.reply_private_t(
        "honeypot-list",
        &Args::new().with("channels", text.join(", ")),
    )
    .await
}

/// Makes a channel with a warning in it and sets it as a honeypot.
async fn create(cx: CommandCx, args: Create) -> Result<(), UserError> {
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let name = args
        .name
        .unwrap_or_else(|| cx.t("honeypot-channel-name", &Args::new()));
    let reason = Reason::truncated(&cx.t("honeypot-reason-create", &Args::new()));
    let created = ctx
        .api
        .execute(Action::CreateChannel {
            guild_id: ctx.guild_id,
            spec: NewChannel {
                name,
                kind: ChannelKind::Text,
                parent_id: None,
                topic: Some(cx.t("honeypot-topic", &Args::new())),
                nsfw: false,
                slowmode_seconds: 0,
                overwrites: Vec::new(),
            },
            reason,
        })
        .await
        .map_err(|error| {
            tracing::warn!(%error, "could not create the honeypot channel");
            UserError::message("honeypot-failed")
        })?;
    let Outcome::Channel(channel) = created else {
        return Err(UserError::message("honeypot-failed"));
    };
    cx.store()
        .set_add(CHANNELS, channel.id.get())
        .await
        .map_err(|e| failed(&e))?;
    if let Err(error) = ctx
        .api
        .send_message(channel.id, cx.t("honeypot-warning", &Args::new()))
        .await
    {
        tracing::warn!(%error, "could not post the warning in the honeypot channel");
    }
    cx.reply_private_t(
        "honeypot-created",
        &Args::new().with("channel", format!("<#{}>", channel.id)),
    )
    .await
}

/// Whether the author must be left alone: the bot, the owner, the staff and the listed members.
async fn exempt(ctx: &EventContext, config: &Config, message: &MessageInfo) -> bool {
    let author = message.author.id;
    if Some(author) == ctx.bot() || ids(&config.exempt_users, UserId::new_checked).contains(&author)
    {
        return true;
    }
    if ids(&config.exempt_roles, RoleId::new_checked)
        .iter()
        .any(|role| message.author_roles.contains(role))
    {
        return true;
    }
    // A moderator by the server's own settings has no power of Discord's to show for it.
    let actor = Actor {
        user: author,
        roles: message.author_roles.clone(),
        administrator: false,
        owner: false,
    };
    if ctx.permissions().level_of(&actor) >= Level::MODERATOR {
        return true;
    }
    match ctx.api.get_guild(ctx.guild_id).await {
        Ok(guild) if guild.owner_id == author => return true,
        Ok(_) => {}
        // Not knowing who the owner is, nobody is punished.
        Err(_) => return true,
    }
    match ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
    {
        Ok(Outcome::Roles(roles)) => roles
            .iter()
            .filter(|role| message.author_roles.contains(&role.id))
            .any(|role| role.permissions.intersects(STAFF)),
        _ => true,
    }
}

/// Somebody posted: in a honeypot channel they are punished and the message is deleted.
async fn posted(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot || message.from_webhook {
        return Ok(Flow::Continue);
    }
    let trap = ctx
        .store()
        .set_has(CHANNELS, message.channel_id.get())
        .await
        .map_err(|e| ModuleError::other(e.to_string()))?;
    if !trap {
        return Ok(Flow::Continue);
    }
    let config: Config = parse_module_config(&ctx)?;
    if exempt(&ctx, &config, &message).await {
        return Ok(Flow::Continue);
    }
    let reason = ctx.t_in(ctx.locale, "honeypot-reason", &Args::new());
    if let Err(error) = ctx
        .api
        .delete_message(message.channel_id, message.id, Reason::truncated(&reason))
        .await
    {
        tracing::debug!(%error, "could not delete the message in the honeypot");
    }
    let settings = Settings {
        dm: false,
        ..Settings::default()
    };
    let punisher = Punisher::new(&ctx, Moderator::bot(&ctx, "honeypot")?, settings)
        .source(closure_sdk::CaseSource::Automod);
    let target = message.author.id;
    let done = match config.action {
        Method::Ban => punisher
            .ban(target, None, config.delete_seconds(), Some(reason))
            .await
            .map(drop),
        Method::Kick => punisher.kick(target, Some(reason)).await.map(drop),
        Method::Timeout => punisher
            .timeout(target, config.timeout(), Some(reason))
            .await
            .map(drop),
    };
    if let Err(error) = done {
        tracing::warn!(?error, "could not punish whoever posted in a honeypot");
    }
    Ok(Flow::Continue)
}

fn config_schema() -> serde_json::Value {
    let snowflakes = |description: &str, widget: &str| {
        json!({
            "type": "array",
            "maxItems": 50,
            "items": { "type": "string", "pattern": "^[0-9]{1,20}$" },
            "description": description,
            "x-ui": { "level": "basic", "widget": widget }
        })
    };
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {
                "type": "string",
                "enum": ["ban", "kick", "timeout"],
                "description": "config-honeypot-action",
                "x-ui": { "level": "basic" }
            },
            "delete_hours": {
                "type": "integer",
                "minimum": 0,
                "maximum": MAX_DELETE_HOURS,
                "description": "config-honeypot-delete_hours",
                "x-ui": { "level": "basic" }
            },
            "timeout": {
                "type": "string",
                "pattern": "^([0-9]{1,5}[smhdw] ?)+$",
                "description": "config-honeypot-timeout",
                "x-ui": { "level": "basic" }
            },
            "exempt_users": snowflakes("config-honeypot-exempt_users", "user"),
            "exempt_roles": snowflakes("config-honeypot-exempt_roles", "role")
        }
    })
}

/// The module before it is built, so that tests can run it on the SDK's pretend Discord.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let defaults = json!({
        "action": "ban",
        "delete_hours": 24,
        "timeout": "1d",
        "exempt_users": [],
        "exempt_roles": []
    });
    Ok(punish::jobs(
        ModuleBuilder::new("honeypot", "Honeypot", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::Storage)
            .capability(Capability::Schedule)
            .capability(Capability::Cases)
            .capability(Capability::BanMembers)
            .capability(Capability::KickMembers)
            .capability(Capability::ReadBans)
            .capability(Capability::TimeoutMembers)
            .capability(Capability::ManageMessages)
            .capability(Capability::ManageChannels)
            .capability(Capability::SendMessages)
            .capability(Capability::ReadGuild)
            .intents(Intents::GUILD_MESSAGES)
            .permission(PermissionSpec::new(
                "honeypot.manage",
                Level::ADMIN,
                "permission-honeypot-manage",
            )?)
            .config(config_schema(), defaults)
            .on(Subscription::new(EventKind::MessageCreated), posted)
            .group(
                "honeypot",
                [
                    Command::<Pick>::new("add")
                        .requires("honeypot.manage")
                        .handler(add),
                    Command::<Pick>::new("remove")
                        .requires("honeypot.manage")
                        .handler(remove),
                    Command::<()>::new("list")
                        .requires("honeypot.manage")
                        .handler(list),
                    Command::<Create>::new("create")
                        .requires("honeypot.manage")
                        .handler(create),
                ],
            )
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    ))
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
