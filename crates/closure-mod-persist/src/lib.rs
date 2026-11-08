//! Gives a member back what they had when they come back: the roles the server chose to keep, and
//! their nickname.
//!
//! The bot cannot read a member who has left, so it keeps a note of each member's roles and
//! nickname as they change, and reads it when the member joins again. A member nobody changed
//! since the bot started has no note yet.

use std::time::Duration;

use closure_core::action::Reason;
use closure_core::error::DiscordError;
use closure_core::event::{Event, EventKind, MemberInfo, RoleId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, CoreError, Intents, Locale, ModuleBuilder, ModuleError, SdkModule, Subscription,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// How long a note of a member is kept after the last change.
const KEEP: Duration = Duration::from_hours(2160);

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    roles: Vec<String>,
    nickname: bool,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Note {
    roles: Vec<u64>,
    nick: Option<String>,
    /// A giving back that did not finish: what the member has now is not all they had, and the
    /// note is not made smaller by it.
    #[serde(default)]
    restoring: bool,
}

fn stored(error: &closure_core::storage::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn key(user: u64) -> String {
    format!("note:{user}")
}

impl Config {
    fn kept_roles(&self, member: &MemberInfo) -> Vec<u64> {
        let kept: Vec<u64> = self.roles.iter().filter_map(|r| r.parse().ok()).collect();
        member
            .roles
            .iter()
            .map(|r| r.get())
            .filter(|r| kept.contains(r))
            .collect()
    }
}

async fn changed(ctx: &EventContext, member: &MemberInfo) -> Result<(), ModuleError> {
    let config: Config = ctx.parse_config()?;
    let mut note = Note {
        roles: config.kept_roles(member),
        nick: member.nick.clone().filter(|_| config.nickname),
        restoring: false,
    };
    let key = key(member.user.id.get());
    // While a giving back is not finished, the note keeps what was to be given, and adds what the
    // member has now: the roles that were given back would otherwise make the rest forgotten.
    if let Some(old) = ctx
        .store()
        .get::<Note>(&key)
        .await
        .map_err(|e| stored(&e))?
        .filter(|old| old.restoring)
        && old.roles.iter().any(|role| !note.roles.contains(role))
    {
        let mut roles = old.roles;
        for role in &note.roles {
            if !roles.contains(role) {
                roles.push(*role);
            }
        }
        note.roles = roles;
        note.nick = note.nick.or(old.nick);
        note.restoring = true;
    }
    if note == Note::default() {
        ctx.store().delete(&key).await.map_err(|e| stored(&e))?;
    } else {
        ctx.store()
            .set_for(&key, &note, KEEP)
            .await
            .map_err(|e| stored(&e))?;
    }
    Ok(())
}

/// Runs `attempt` again, twice at most, while it fails in a way that may pass.
async fn again<F, Fut, T>(mut attempt: F) -> Result<T, DiscordError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DiscordError>>,
{
    let mut tries = 0;
    loop {
        match attempt().await {
            Err(error) if error.is_retryable() && tries < 2 => tries += 1,
            other => return other,
        }
    }
}

async fn joined(ctx: &EventContext, member: &MemberInfo) -> Result<(), ModuleError> {
    let config: Config = ctx.parse_config()?;
    let key = key(member.user.id.get());
    let Some(note) = ctx
        .store()
        .get::<Note>(&key)
        .await
        .map_err(|e| stored(&e))?
    else {
        return Ok(());
    };
    let note_back = Note {
        roles: note.roles.clone(),
        nick: note.nick.clone(),
        restoring: false,
    };
    let reason = Reason::new(ctx.t_in(ctx.locale, "persist-reason", &Args::new()))?;
    let allowed: Vec<u64> = config.roles.iter().filter_map(|r| r.parse().ok()).collect();
    // A failure that may pass (a busy Discord) is tried again, and if it stays, the snapshot is
    // kept for the next time; one that will not (a role above the bot's) is only noted.
    let mut pending: Option<DiscordError> = None;
    for role in note.roles.iter().filter(|r| allowed.contains(r)) {
        let given = again(|| {
            ctx.api.add_role(
                ctx.guild_id,
                member.user.id,
                RoleId::new(*role),
                reason.clone(),
            )
        })
        .await;
        if let Err(error) = given {
            tracing::debug!(?error, guild = %ctx.guild_id, "could not give a role back");
            if error.is_retryable() {
                pending = Some(error);
            }
        }
    }
    if config.nickname
        && let Some(nick) = note.nick
    {
        let set = again(|| {
            ctx.api.execute(closure_core::action::Action::SetNickname {
                guild_id: ctx.guild_id,
                user_id: member.user.id,
                nickname: Some(nick.clone()),
                reason: reason.clone(),
            })
        })
        .await;
        if let Err(error) = set {
            tracing::debug!(?error, guild = %ctx.guild_id, "could not give a nickname back");
            if error.is_retryable() {
                pending = Some(error);
            }
        }
    }
    if let Some(error) = pending {
        // Kept for the next time, and marked: what the member has now must not shrink it.
        let unfinished = Note {
            restoring: true,
            ..note_back
        };
        ctx.store()
            .set_for(&key, &unfinished, KEEP)
            .await
            .map_err(|e| stored(&e))?;
        return Err(error.into());
    }
    ctx.store().delete(&key).await.map_err(|e| stored(&e))?;
    Ok(())
}

async fn on_member(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    match &event {
        Event::MemberUpdated(member) if !member.user.bot => changed(&ctx, member).await?,
        Event::MemberJoined(member) if !member.user.bot => joined(&ctx, member).await?,
        _ => {}
    }
    Ok(Flow::Continue)
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "roles": {
                "type": "array",
                "maxItems": 25,
                "items": {"type": "string", "pattern": "^[0-9]{1,20}$"},
                "description": "config-persist-roles",
                "x-ui": { "level": "basic", "widget": "role" }
            },
            "nickname": {
                "type": "boolean",
                "description": "config-persist-nickname",
                "x-ui": { "level": "basic" }
            }
        }
    });
    Ok(
        ModuleBuilder::new("persist", "Persist", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::ManageRoles)
            .capability(closure_sdk::Capability::ManageNicknames)
            .capability(closure_sdk::Capability::Storage)
            .intents(Intents::GUILD_MEMBERS)
            .config(schema, json!({"roles": [], "nickname": false}))
            .on(Subscription::new(EventKind::MemberUpdated), |ctx, event| {
                on_member(ctx, event)
            })
            .on(Subscription::new(EventKind::MemberJoined), |ctx, event| {
                on_member(ctx, event)
            })
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
