//! Sticky messages: a text that stays at the bottom of a channel.
//!
//! When somebody speaks in a channel that has a sticky text, the module waits a few seconds (so
//! that a busy channel is not answered message by message), deletes the sticky message it posted
//! last, and posts it again. The id of the posted message is kept in the module's storage.
//!
//! Messages of bots and webhooks do not move the sticky text; that keeps two bots from chasing
//! each other.

use std::time::Duration;

use closure_core::action::Reason;
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, Event, EventKind, MessageId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::duration::parse as parse_duration;
use closure_sdk::{
    Args, Capability, CoreError, Intents, JobCx, Locale, ModuleBuilder, ModuleError, SdkModule,
    Subscription, parse_module_config,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const REPOST: &str = "repost";
/// Discord's limit for the text of a message.
const MAX_TEXT: usize = 2_000;
const MAX_STICKIES: usize = 25;
/// Posted messages not yet deleted; a repost waits while this many are left.
const MAX_STALE: usize = 20;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    stickies: Vec<Sticky>,
    delay: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            stickies: Vec::new(),
            delay: "5s".to_owned(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Sticky {
    channel: String,
    text: String,
}

impl Config {
    fn text_in(&self, channel: ChannelId) -> Option<&str> {
        self.stickies
            .iter()
            .find(|sticky| sticky.channel.parse() == Ok(channel.get()))
            .map(|sticky| sticky.text.as_str())
    }

    fn delay(&self) -> Duration {
        parse_duration(&self.delay)
            .unwrap_or(Duration::from_secs(5))
            .clamp(Duration::from_secs(1), Duration::from_hours(1))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Repost {
    channel: u64,
}

fn job_key(channel: ChannelId) -> String {
    format!("repost:{channel}")
}

fn last_key(channel: ChannelId) -> String {
    format!("last:{channel}")
}

fn stale_key(channel: ChannelId) -> String {
    format!("stale:{channel}")
}

fn stored(error: &closure_core::storage::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

/// Somebody spoke: if the channel has a sticky text, it is posted again soon.
async fn spoke(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot || message.from_webhook {
        return Ok(Flow::Continue);
    }
    let config: Config = parse_module_config(&ctx)?;
    if config.text_in(message.channel_id).is_none() {
        return Ok(Flow::Continue);
    }
    ctx.scheduler()
        .job(
            REPOST,
            &Repost {
                channel: message.channel_id.get(),
            },
        )
        .map_err(|error| ModuleError::other(error.to_string()))?
        .key(&job_key(message.channel_id))
        // A job that is waiting already will post the text after this message too; one that is
        // running may have posted it already, and is run again.
        .unless_waiting()
        .after(config.delay())
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(Flow::Continue)
}

async fn repost(cx: JobCx) -> Result<(), ModuleError> {
    let payload: Repost = cx.payload()?;
    let Some(channel) = ChannelId::new_checked(payload.channel) else {
        return Ok(());
    };
    let ctx = cx.ctx();
    let config: Config = parse_module_config(ctx)?;
    // The text may have been taken off the channel while the job waited.
    let Some(text) = config.text_in(channel) else {
        return Ok(());
    };
    let store = ctx.store();
    let mut stale: Vec<u64> = store
        .get(&stale_key(channel))
        .await
        .map_err(|e| stored(&e))?
        .unwrap_or_default();
    let last: Option<u64> = store
        .get(&last_key(channel))
        .await
        .map_err(|e| stored(&e))?;
    if let Some(old) = last.filter(|old| !stale.contains(old)) {
        stale.push(old);
    }
    // Listed before anything is deleted, so that a failed delete leaves it to a later repost.
    store
        .set(&stale_key(channel), &stale)
        .await
        .map_err(|e| stored(&e))?;
    let mut pending = Vec::new();
    for id in stale {
        let Some(message) = MessageId::new_checked(id) else {
            continue;
        };
        let reason = Reason::truncated(&ctx.t_in(ctx.locale, "sticky-reason", &Args::new()));
        match ctx.api.delete_message(channel, message, reason).await {
            Ok(()) | Err(DiscordError::NotFound) => {}
            Err(error) => {
                tracing::warn!(%error, "could not delete an old sticky message");
                pending.push(id);
            }
        }
    }
    if pending.len() >= MAX_STALE {
        store
            .set(&stale_key(channel), &pending)
            .await
            .map_err(|e| stored(&e))?;
        return Err(ModuleError::other(
            "too many sticky messages could not be deleted; the repost waits",
        ));
    }
    let posted = ctx.api.send_message(channel, text).await?;
    // Listed before it becomes the last, so that a failed write of last does not leave it untracked.
    pending.push(posted.id.get());
    store
        .set(&stale_key(channel), &pending)
        .await
        .map_err(|e| stored(&e))?;
    store
        .set(&last_key(channel), &posted.id.get())
        .await
        .map_err(|e| stored(&e))?;
    Ok(())
}

/// The module before it is built, so that tests can run it on the SDK's pretend Discord.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "stickies": {
                "type": "array",
                "maxItems": MAX_STICKIES,
                "description": "config-sticky-stickies",
                "x-ui": { "level": "basic" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["channel", "text"],
                    "properties": {
                        "channel": { "type": "string", "pattern": "^[0-9]{1,20}$" },
                        "text": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT }
                    }
                }
            },
            "delay": {
                "type": "string",
                "pattern": "^([0-9]{1,5}[smh] ?)+$",
                "description": "config-sticky-delay",
                "x-ui": { "level": "basic" }
            }
        }
    });
    let defaults = json!({ "stickies": [], "delay": "5s" });
    Ok(
        ModuleBuilder::new("sticky", "Sticky", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::SendMessages)
            .capability(Capability::ManageMessages)
            .capability(Capability::Storage)
            .capability(Capability::Schedule)
            .intents(Intents::GUILD_MESSAGES)
            .config(schema, defaults)
            .on(Subscription::new(EventKind::MessageCreated), spoke)
            .job(REPOST, repost)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
