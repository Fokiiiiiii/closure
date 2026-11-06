//! A text channel that only the people in a voice channel can see.
//!
//! Each pair names a voice channel and a text channel. When a member joins the voice channel they
//! get to see and write in the text channel, and lose it when they leave. The text channel is
//! hidden from everyone else by its own permissions, which the server sets.

use closure_core::action::{Action, OverwriteSpec, OverwriteTarget, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, Event, EventKind};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, CoreError, Intents, Locale, ModuleBuilder, ModuleError, SdkModule, Subscription,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::guild::Permissions;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    pairs: Vec<Pair>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    voice: String,
    text: String,
}

impl Config {
    /// The text channel that belongs to a voice channel.
    fn text_of(&self, voice: ChannelId) -> Option<ChannelId> {
        self.pairs
            .iter()
            .find(|p| p.voice.parse::<u64>().ok() == Some(voice.get()))
            .and_then(|p| p.text.parse::<u64>().ok())
            // A channel is never 0: a setting that says so names nothing, and must not panic.
            .filter(|id| *id > 0)
            .map(ChannelId::new)
    }
}

const SEEN: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::SEND_MESSAGES)
    .union(Permissions::READ_MESSAGE_HISTORY);

async fn on_voice(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::VoiceStateChanged {
        user,
        before,
        after,
        ..
    } = &event
    else {
        return Ok(Flow::Continue);
    };
    if user.bot || before == after {
        return Ok(Flow::Continue);
    }
    let config: Config = ctx.parse_config()?;
    let reason = Reason::new(ctx.t_in(ctx.locale, "companion-reason", &Args::new()))?;
    let target = OverwriteTarget::Member(user.id);
    let left = before.and_then(|c| config.text_of(c));
    let entered = after.and_then(|c| config.text_of(c));
    if let Some(text) = left.filter(|text| Some(*text) != entered) {
        let result = ctx
            .api
            .execute(Action::DeletePermissionOverwrite {
                channel_id: text,
                target,
                reason: reason.clone(),
            })
            .await;
        note(&result, "take a text channel away");
    }
    if let Some(text) = entered {
        let result = ctx
            .api
            .execute(Action::SetPermissionOverwrite {
                channel_id: text,
                overwrite: OverwriteSpec {
                    target,
                    allow: SEEN,
                    deny: Permissions::empty(),
                },
                reason,
            })
            .await;
        note(&result, "show a text channel");
    }
    Ok(Flow::Continue)
}

fn note<T>(result: &Result<T, DiscordError>, what: &str) {
    if let Err(error) = result {
        tracing::debug!(?error, "could not {what}");
    }
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let channel = json!({"type": "string", "pattern": "^[0-9]{1,20}$"});
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "pairs": {
                "type": "array",
                "maxItems": 25,
                "description": "config-companion-pairs",
                "x-ui": { "level": "basic" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["voice", "text"],
                    "properties": {
                        "voice": channel.clone(),
                        "text": channel
                    }
                }
            }
        }
    });
    Ok(
        ModuleBuilder::new("companion", "Companion channels", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::ManageChannels)
            .intents(Intents::GUILD_VOICE_STATES)
            .config(schema, json!({"pairs": []}))
            .on(Subscription::new(EventKind::VoiceStateChanged), on_voice)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
