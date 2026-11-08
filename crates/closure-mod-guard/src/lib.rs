//! A guard against a nuke: an account that deletes channels or roles, or bans, faster than anyone on purpose.
//!
//! Changes are counted per account in a short window; a limit takes away its moderator roles (a bot is kicked).
//! The owner, the bot, and the trusted accounts and roles are never touched.
//!
//! The counts are kept in memory: a window is a half minute, so a restart forgets at most that.

use std::collections::HashMap;
use std::sync::Mutex;

use closure_core::action::{Action, AuditLogQuery, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, Event, EventKind, GuildId, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::duration::{parse as parse_duration, snowflake_time};
use closure_sdk::{
    Args, Capability, CoreError, Intents, Locale, ModuleBuilder, ModuleError, SdkModule,
    Subscription, parse_module_config,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::guild::Permissions;
use twilight_model::guild::audit_log::{AuditLogEntry, AuditLogEventType};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

/// How old an audit log entry may be and still be the change an event reports.
const AUDIT_WINDOW_SECONDS: i64 = 60;
const AUDIT_LOOKUP_LIMIT: u16 = 50;
/// How long an account that was acted on is not acted on again, so that the rest of a burst that
/// is already stopped does not tell the staff once for each change.
const QUIET: i64 = 600;
/// The most accounts followed at once; past that the counts start over.
const MAX_FOLLOWED: usize = 10_000;

/// The powers that make a role one to take away from an account that nukes.
const POWERS: Permissions = Permissions::ADMINISTRATOR
    .union(Permissions::MANAGE_GUILD)
    .union(Permissions::MANAGE_ROLES)
    .union(Permissions::MANAGE_CHANNELS)
    .union(Permissions::MANAGE_WEBHOOKS)
    .union(Permissions::BAN_MEMBERS)
    .union(Permissions::KICK_MEMBERS)
    .union(Permissions::MODERATE_MEMBERS)
    .union(Permissions::MANAGE_MESSAGES);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Change {
    ChannelDelete,
    RoleDelete,
    Ban,
}

impl Change {
    fn audit(self) -> AuditLogEventType {
        match self {
            Self::ChannelDelete => AuditLogEventType::ChannelDelete,
            Self::RoleDelete => AuditLogEventType::RoleDelete,
            Self::Ban => AuditLogEventType::MemberBanAdd,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::ChannelDelete => "guard-change-channel",
            Self::RoleDelete => "guard-change-role",
            Self::Ban => "guard-change-ban",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Response {
    #[default]
    Strip,
    Alert,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Config {
    /// Channels deleted by one account within the window; 0: not watched.
    channel_deletes: u32,
    role_deletes: u32,
    bans: u32,
    window: String,
    response: Response,
    /// Where the staff are told; else the log channel of the server.
    alert_channel: Option<String>,
    exempt_users: Vec<String>,
    exempt_roles: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            channel_deletes: 3,
            role_deletes: 3,
            bans: 10,
            window: "30s".to_owned(),
            response: Response::Strip,
            alert_channel: None,
            exempt_users: Vec::new(),
            exempt_roles: Vec::new(),
        }
    }
}

impl Config {
    fn limit(&self, change: Change) -> u32 {
        match change {
            Change::ChannelDelete => self.channel_deletes,
            Change::RoleDelete => self.role_deletes,
            Change::Ban => self.bans,
        }
    }

    fn window(&self) -> i64 {
        parse_duration(&self.window)
            .map_or(30, |window| i64::try_from(window.as_secs()).unwrap_or(30))
            .clamp(1, 3_600)
    }
}

fn ids<T>(list: &[String], make: impl Fn(u64) -> Option<T>) -> Vec<T> {
    list.iter()
        .filter_map(|text| text.parse().ok())
        .filter_map(make)
        .collect()
}

type Followed = HashMap<(GuildId, UserId, Change), Vec<i64>>;
type Quiet = HashMap<(GuildId, UserId), i64>;

static COUNTS: Mutex<Option<Followed>> = Mutex::new(None);
static ACTED: Mutex<Option<Quiet>> = Mutex::new(None);

/// Counts one more change by `user` and says how many there are in the window now.
fn count(guild: GuildId, user: UserId, change: Change, now: i64, window: i64) -> usize {
    let mut counts = COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = counts.get_or_insert_with(HashMap::new);
    if map.len() >= MAX_FOLLOWED {
        map.clear();
    }
    let times = map.entry((guild, user, change)).or_default();
    times.retain(|at| now - *at < window);
    times.push(now);
    times.len()
}

/// Forgets the counts of `user`, whose powers were just dealt with.
fn forget(guild: GuildId, user: UserId) {
    if let Some(map) = COUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        map.retain(|(g, u, _), _| (*g, *u) != (guild, user));
    }
}

/// Takes a turn at acting on `user`: false when it was acted on a moment ago.
fn first_to_act(guild: GuildId, user: UserId, now: i64) -> bool {
    let mut acted = ACTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = acted.get_or_insert_with(HashMap::new);
    if map.len() >= MAX_FOLLOWED {
        map.clear();
    }
    if map.get(&(guild, user)).is_some_and(|at| now - *at < QUIET) {
        return false;
    }
    map.insert((guild, user), now);
    true
}

fn change_of(event: &Event) -> Option<(Change, u64)> {
    match event {
        Event::ChannelDeleted(channel) => Some((Change::ChannelDelete, channel.id.get())),
        Event::RoleDeleted { role_id, .. } => Some((Change::RoleDelete, role_id.get())),
        Event::BanAdded { user, .. } => Some((Change::Ban, user.id.get())),
        _ => None,
    }
}

async fn actor_of(ctx: &EventContext, change: Change, target: u64) -> Option<UserId> {
    let outcome = ctx
        .api
        .execute(Action::GetAuditLog {
            guild_id: ctx.guild_id,
            query: AuditLogQuery {
                user_id: None,
                action_type: Some(u16::from(change.audit())),
                before: None,
                limit: Some(AUDIT_LOOKUP_LIMIT),
            },
        })
        .await;
    let Ok(Outcome::AuditLog(log)) = outcome else {
        tracing::warn!(guild = ctx.guild_id.get(), "could not read the audit log");
        return None;
    };
    let now = ctx.now();
    log.entries
        .iter()
        .find(|entry: &&AuditLogEntry| {
            entry.target_id.is_some_and(|id| id.get() == target)
                && now - snowflake_time(entry.id.get()) <= AUDIT_WINDOW_SECONDS
        })
        .and_then(|entry| entry.user_id)
}

struct Stopped {
    stripped: Vec<RoleId>,
    kept: usize,
    kicked: bool,
}

/// Takes the powers of `user` away: every role of it with moderator powers; a bot is kicked.
async fn strip(ctx: &EventContext, user: UserId, reason: &str) -> Result<Stopped, DiscordError> {
    let member = ctx.api.get_member(ctx.guild_id, user).await?;
    let mut done = Stopped {
        stripped: Vec::new(),
        kept: 0,
        kicked: false,
    };
    if member.user.bot {
        ctx.api
            .kick(ctx.guild_id, user, Reason::truncated(reason))
            .await?;
        done.kicked = true;
        return Ok(done);
    }
    let Outcome::Roles(known) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await?
    else {
        return Ok(done);
    };
    for role in &member.roles {
        let powerful = known
            .iter()
            .find(|known| known.id == *role)
            .is_some_and(|known| known.permissions.intersects(POWERS));
        if !powerful {
            continue;
        }
        match ctx
            .api
            .remove_role(ctx.guild_id, user, *role, Reason::truncated(reason))
            .await
        {
            Ok(()) => done.stripped.push(*role),
            Err(error) => {
                tracing::warn!(%error, role = role.get(), "could not take a role off");
                done.kept += 1;
            }
        }
    }
    Ok(done)
}

async fn alert(
    ctx: &EventContext,
    config: &Config,
    (user, change, seen): (UserId, Change, usize),
    done: Option<&Stopped>,
) {
    let channel = config
        .alert_channel
        .as_deref()
        .and_then(|text| text.parse::<u64>().ok())
        .and_then(ChannelId::new_checked)
        .or(ctx.policy().log_channel);
    let Some(channel) = channel else {
        tracing::warn!(user = user.get(), "no channel to tell the staff of a nuke");
        return;
    };
    let say = |key: &str, args: &Args| ctx.t_in(ctx.locale, key, args);
    let mut text = say(
        "guard-alert",
        &Args::new()
            .with("user", format!("<@{user}>"))
            .with("change", say(change.key(), &Args::new()))
            .with("count", u64::try_from(seen).unwrap_or(u64::MAX))
            .with("seconds", config.window()),
    );
    if let Some(done) = done {
        text.push('\n');
        text.push_str(&if done.kicked {
            say("guard-kicked", &Args::new())
        } else {
            say(
                "guard-stripped",
                &Args::new()
                    .with("done", u64::try_from(done.stripped.len()).unwrap_or(0))
                    .with("kept", u64::try_from(done.kept).unwrap_or(0)),
            )
        });
    }
    if let Err(error) = ctx.api.send_message(channel, text).await {
        tracing::warn!(%error, "could not tell the staff of a nuke");
    }
}

async fn exempt(ctx: &EventContext, config: &Config, user: UserId) -> bool {
    if Some(user) == ctx.bot() || ids(&config.exempt_users, UserId::new_checked).contains(&user) {
        return true;
    }
    match ctx.api.get_guild(ctx.guild_id).await {
        Ok(guild) if guild.owner_id == user => return true,
        Ok(_) => {}
        // Not knowing the owner is not a reason to strip someone.
        Err(_) => return true,
    }
    let trusted = ids(&config.exempt_roles, RoleId::new_checked);
    if trusted.is_empty() {
        return false;
    }
    match ctx.api.get_member(ctx.guild_id, user).await {
        Ok(member) => member.roles.iter().any(|role| trusted.contains(role)),
        // An account that is gone cannot be stripped; one that cannot be read is left alone.
        Err(_) => true,
    }
}

async fn on_event(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Some((change, target)) = change_of(&event) else {
        return Ok(Flow::Continue);
    };
    let config: Config = parse_module_config(&ctx)?;
    let limit = config.limit(change);
    if limit == 0 {
        return Ok(Flow::Continue);
    }
    let Some(user) = actor_of(&ctx, change, target).await else {
        return Ok(Flow::Continue);
    };
    let seen = count(ctx.guild_id, user, change, ctx.now(), config.window());
    if seen < usize::try_from(limit).unwrap_or(usize::MAX) {
        return Ok(Flow::Continue);
    }
    if exempt(&ctx, &config, user).await || !first_to_act(ctx.guild_id, user, ctx.now()) {
        return Ok(Flow::Continue);
    }
    forget(ctx.guild_id, user);
    let reason = ctx.t_in(ctx.locale, "guard-reason", &Args::new());
    let done = match config.response {
        Response::Strip => match strip(&ctx, user, &reason).await {
            Ok(done) => Some(done),
            Err(error) => {
                tracing::warn!(%error, "could not stop an account that nukes");
                Some(Stopped {
                    stripped: Vec::new(),
                    kept: 1,
                    kicked: false,
                })
            }
        },
        Response::Alert => None,
    };
    alert(&ctx, &config, (user, change, seen), done.as_ref()).await;
    Ok(Flow::Continue)
}

fn snowflake_list(description: &str) -> serde_json::Value {
    json!({
        "type": "array",
        "maxItems": 50,
        "items": { "type": "string", "pattern": "^[0-9]{1,20}$" },
        "description": description,
        "x-ui": { "level": "basic" }
    })
}

fn limit(description: &str, default: u32) -> serde_json::Value {
    json!({
        "type": "integer",
        "minimum": 0,
        "maximum": 1000,
        "default": default,
        "description": description,
        "x-ui": { "level": "basic" }
    })
}

/// The module before it is built, so that tests can run it on the SDK's pretend Discord.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "channel_deletes": limit("config-guard-channel_deletes", 3),
            "role_deletes": limit("config-guard-role_deletes", 3),
            "bans": limit("config-guard-bans", 10),
            "window": {
                "type": "string",
                "pattern": "^([0-9]{1,5}[smh] ?)+$",
                "description": "config-guard-window",
                "x-ui": { "level": "basic" }
            },
            "response": {
                "type": "string",
                "enum": ["strip", "alert"],
                "description": "config-guard-response",
                "x-ui": { "level": "basic" }
            },
            "alert_channel": {
                "anyOf": [{ "type": "string", "pattern": "^[0-9]{1,20}$" }, { "type": "null" }],
                "description": "config-guard-alert_channel",
                "x-ui": { "level": "basic", "widget": "channel" }
            },
            "exempt_users": snowflake_list("config-guard-exempt_users"),
            "exempt_roles": snowflake_list("config-guard-exempt_roles")
        }
    });
    let defaults = json!({
        "channel_deletes": 3,
        "role_deletes": 3,
        "bans": 10,
        "window": "30s",
        "response": "strip",
        "alert_channel": null,
        "exempt_users": [],
        "exempt_roles": []
    });
    Ok(
        ModuleBuilder::new("guard", "Guard", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::ReadAuditLog)
            .capability(Capability::ManageRoles)
            .capability(Capability::KickMembers)
            .capability(Capability::SendMessages)
            .capability(Capability::ReadGuild)
            .intents(Intents::GUILD_MODERATION)
            .config(schema, defaults)
            .on(Subscription::new(EventKind::ChannelDeleted), on_event)
            .on(Subscription::new(EventKind::RoleDeleted), on_event)
            .on(Subscription::new(EventKind::BanAdded), on_event)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
