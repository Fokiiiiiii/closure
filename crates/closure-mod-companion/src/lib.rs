//! A text channel that only the people in a voice channel can see.
//!
//! Each pair names a voice channel and a text channel. When a member joins the voice channel they
//! get to see and write in the text channel, and lose it when they leave. The text channel is
//! hidden from everyone else by its own permissions, which the server sets.

use closure_core::action::{Action, Outcome, OverwriteSpec, OverwriteTarget, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, Event, EventKind, UserId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, CoreError, Intents, Locale, ModuleBuilder, ModuleError, SdkModule, Subscription,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
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

/// The names of the scores that keep what a member's own overwrite of a text channel was before it
/// was made to show the channel: the allowed and the denied bits, each plus one, so that none is
/// told from a score that is not there.
fn kept_allow(text: ChannelId) -> String {
    format!("kept-allow-{}", text.get())
}

fn kept_deny(text: ChannelId) -> String {
    format!("kept-deny-{}", text.get())
}

fn to_score(bits: Permissions) -> i64 {
    i64::try_from(bits.bits())
        .unwrap_or(i64::MAX - 1)
        .saturating_add(1)
}

fn from_score(score: i64) -> Permissions {
    Permissions::from_bits_truncate(u64::try_from(score.saturating_sub(1)).unwrap_or(0))
}

/// The member's overwrite of the text channel as it is now, when there is one.
async fn current_overwrite(
    ctx: &EventContext,
    text: ChannelId,
    user: UserId,
) -> Option<OverwriteSpec> {
    let Ok(Outcome::Channel(channel)) = ctx
        .api
        .execute(Action::GetChannel { channel_id: text })
        .await
    else {
        return None;
    };
    channel
        .permission_overwrites
        .as_deref()?
        .iter()
        .find(|item| item.kind == PermissionOverwriteType::Member && item.id.get() == user.get())
        .map(|item| OverwriteSpec {
            target: OverwriteTarget::Member(user),
            allow: item.allow,
            deny: item.deny,
        })
}

#[allow(
    clippy::too_many_lines,
    reason = "one flow: give back or take away on leaving, keep and show on entering"
)]
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
        // What the member had of their own before is given back; otherwise the overwrite goes.
        let allow = ctx
            .store()
            .score(&kept_allow(text), user.id)
            .await
            .map_err(|e| ModuleError::other(e.to_string()))?;
        let result = if allow > 0 {
            let deny = ctx
                .store()
                .score(&kept_deny(text), user.id)
                .await
                .map_err(|e| ModuleError::other(e.to_string()))?;
            ctx.api
                .execute(Action::SetPermissionOverwrite {
                    channel_id: text,
                    overwrite: OverwriteSpec {
                        target,
                        allow: from_score(allow),
                        deny: from_score(deny),
                    },
                    reason: reason.clone(),
                })
                .await
                .map(drop)
        } else {
            ctx.api
                .execute(Action::DeletePermissionOverwrite {
                    channel_id: text,
                    target,
                    reason: reason.clone(),
                })
                .await
                .map(drop)
        };
        note(&result, "take a text channel away");
        if result.is_ok() {
            for name in [kept_allow(text), kept_deny(text)] {
                ctx.store()
                    .score_put(&name, user.id, 0)
                    .await
                    .map_err(|e| ModuleError::other(e.to_string()))?;
            }
        }
    }
    if let Some(text) = entered {
        // The member's own overwrite is kept before it is changed, once: a second entering finds
        // the one this module made, which is not theirs to be given back.
        let had_before = ctx
            .store()
            .score(&kept_allow(text), user.id)
            .await
            .map_err(|e| ModuleError::other(e.to_string()))?
            > 0;
        let previous = if had_before {
            None
        } else {
            current_overwrite(&ctx, text, user.id).await
        };
        if let Some(previous) = &previous {
            ctx.store()
                .score_put(&kept_allow(text), user.id, to_score(previous.allow))
                .await
                .map_err(|e| ModuleError::other(e.to_string()))?;
            ctx.store()
                .score_put(&kept_deny(text), user.id, to_score(previous.deny))
                .await
                .map_err(|e| ModuleError::other(e.to_string()))?;
        }
        let (allow, deny) = previous.map_or((SEEN, Permissions::empty()), |previous| {
            (previous.allow | SEEN, previous.deny - SEEN)
        });
        let result = ctx
            .api
            .execute(Action::SetPermissionOverwrite {
                channel_id: text,
                overwrite: OverwriteSpec {
                    target,
                    allow,
                    deny,
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
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::ReadGuild)
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
