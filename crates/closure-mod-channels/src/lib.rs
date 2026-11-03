//! Automatic channel cleanup and reactions.
use closure_core::action::Emoji;
use closure_sdk::{
    Capability, CoreError, DiscordError, Event, EventContext, EventKind, Flow, Intents, JobCx,
    Locale, ModuleBuilder, ModuleError, Reason, SdkModule, Subscription, duration,
    parse_module_config,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use twilight_model::id::Id;

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Config {
    auto_delete: Vec<DeleteRule>,
    auto_react: Vec<ReactRule>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRule {
    channel: String,
    after: String,
    #[serde(default = "yes")]
    skip_pinned: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReactRule {
    channel: String,
    emojis: Vec<String>,
    #[serde(default = "yes")]
    skip_bots: bool,
}
const fn yes() -> bool {
    true
}

impl Config {
    fn validate(&self) -> Result<(), ModuleError> {
        if self.auto_delete.len() > 25 || self.auto_react.len() > 25 {
            return Err(ModuleError::Config("too many channel rules".into()));
        }
        for rule in &self.auto_delete {
            validate_channel(&rule.channel)?;
            let delay =
                duration::parse(&rule.after).map_err(|e| ModuleError::Config(e.to_string()))?;
            if !(Duration::from_secs(5)..=Duration::from_hours(168)).contains(&delay) {
                return Err(ModuleError::Config("auto_delete delay out of range".into()));
            }
        }
        for rule in &self.auto_react {
            validate_channel(&rule.channel)?;
            if !(1..=5).contains(&rule.emojis.len()) {
                return Err(ModuleError::Config("invalid emoji count".into()));
            }
            for value in &rule.emojis {
                emoji(value)?;
            }
        }
        Ok(())
    }
}

fn validate_channel(channel: &str) -> Result<(), ModuleError> {
    if channel.starts_with('0')
        || !channel.bytes().all(|b| b.is_ascii_digit())
        || channel.parse::<u64>().ok().is_none_or(|id| id == 0)
    {
        return Err(ModuleError::Config("invalid channel id".into()));
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct DeleteJob {
    channel: u64,
    message: u64,
    skip_pinned: bool,
}

fn settings() -> (serde_json::Value, serde_json::Value) {
    let channel = json!({"type": "string", "pattern": "^[1-9][0-9]{0,19}$"});
    let schema = json!({"type": "object", "additionalProperties": false, "properties": {
        "auto_delete": {"type": "array", "maxItems": 25, "description": "config-auto-delete", "items": {
            "type": "object", "additionalProperties": false, "required": ["channel", "after"],
            "properties": {"channel": channel, "after": {"type": "string", "pattern": "^[0-9]+[smhdw]( *[0-9]+[smhdw])*$"}, "skip_pinned": {"type": "boolean", "default": true}}
        }},
        "auto_react": {"type": "array", "maxItems": 25, "description": "config-auto-react", "items": {
            "type": "object", "additionalProperties": false, "required": ["channel", "emojis"],
            "properties": {"channel": channel, "emojis": {"type": "array", "minItems": 1, "maxItems": 5, "items": {"type": "string", "minLength": 1, "maxLength": 64}}, "skip_bots": {"type": "boolean", "default": true}}
        }}
    }});
    (schema, json!({"auto_delete": [], "auto_react": []}))
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = settings();
    Ok(
        ModuleBuilder::new("channels", "Channels", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::ManageMessages)
            .capability(Capability::ReadMessages)
            .capability(Capability::AddReactions)
            .intents(Intents::GUILD_MESSAGES)
            .on(Subscription::new(EventKind::MessageCreated), on_event)
            .job("delete", delete)
            .config(schema, defaults)
            .catalog(Locale::En, include_str!("../locales/en.ftl"))
            .catalog(Locale::Ja, include_str!("../locales/ja.ftl")),
    )
}
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

fn emoji(text: &str) -> Result<Emoji, ModuleError> {
    if let Some(body) = text.strip_prefix("<:").or_else(|| text.strip_prefix("<a:")) {
        let (name, id) = body
            .strip_suffix('>')
            .and_then(|s| s.split_once(':'))
            .ok_or_else(|| ModuleError::Config("invalid emoji".into()))?;
        let id = id
            .parse::<u64>()
            .ok()
            .filter(|n| *n != 0)
            .ok_or_else(|| ModuleError::Config("invalid emoji id".into()))?;
        if !(2..=32).contains(&name.len())
            || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            return Err(ModuleError::Config("invalid emoji name".into()));
        }
        return Ok(Emoji::Custom {
            id: Id::new(id),
            name: Some(name.into()),
        });
    }
    let chars: Vec<_> = text.chars().collect();
    if chars.is_empty()
        || chars.len() > 64
        || !chars.iter().enumerate().all(|(i, c)| {
            if c.is_ascii() {
                matches!(c, '0'..='9' | '#' | '*')
                    && chars
                        .get(i + 1)
                        .is_some_and(|c| matches!(c, '\u{fe0f}' | '\u{20e3}'))
            } else {
                !c.is_whitespace() && !c.is_control()
            }
        })
    {
        return Err(ModuleError::Config("invalid emoji".into()));
    }
    Ok(Emoji::Unicode { value: text.into() })
}

async fn on_event(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = event else {
        return Ok(Flow::Continue);
    };
    let config: Config = parse_module_config(&ctx)?;
    config.validate()?;
    let channel = message.channel_id.to_string();
    let mut deletes = config
        .auto_delete
        .iter()
        .filter(|rule| rule.channel == channel);
    if let Some(rule) = deletes.next() {
        if deletes.next().is_some() {
            tracing::warn!("duplicate auto_delete channel; using first rule");
        }
        if ctx.policy().log_channel == Some(message.channel_id) {
            tracing::warn!("ignoring auto_delete for log channel");
        } else {
            let delay =
                duration::parse(&rule.after).map_err(|e| ModuleError::Config(e.to_string()))?;
            ctx.scheduler()
                .job(
                    "delete",
                    &DeleteJob {
                        channel: message.channel_id.get(),
                        message: message.id.get(),
                        skip_pinned: rule.skip_pinned,
                    },
                )
                .map_err(|e| ModuleError::other(e.to_string()))?
                .key(&format!("message:{}", message.id))
                .after(delay)
                .await
                .map_err(|e| ModuleError::other(e.to_string()))?;
        }
    }
    let mut reactions = config
        .auto_react
        .iter()
        .filter(|rule| rule.channel == channel);
    if let Some(rule) = reactions.next() {
        if reactions.next().is_some() {
            tracing::warn!("duplicate auto_react channel; using first rule");
        }
        let emojis = rule
            .emojis
            .iter()
            .map(|s| emoji(s))
            .collect::<Result<Vec<_>, _>>()?;
        if !(rule.skip_bots && message.author.bot) {
            for emoji in emojis {
                if let Err(error) = ctx
                    .api
                    .add_reaction(message.channel_id, message.id, emoji)
                    .await
                {
                    tracing::warn!(?error, "could not add reaction");
                }
            }
        }
    }
    Ok(Flow::Continue)
}

async fn delete(cx: JobCx) -> Result<(), ModuleError> {
    let job: DeleteJob = cx.payload()?;
    let channel = Id::new(job.channel);
    if cx.ctx().policy().log_channel == Some(channel) {
        tracing::warn!("ignoring auto_delete for log channel");
        return Ok(());
    }
    let message = match cx.api().get_message(channel, Id::new(job.message)).await {
        Ok(message) => message,
        Err(DiscordError::NotFound) => return Ok(()),
        Err(error) if error.is_retryable() => return Err(error.into()),
        Err(error) => {
            tracing::warn!(?error, "could not retrieve message for deletion");
            return Ok(());
        }
    };
    if should_delete(job.skip_pinned, message.pinned)
        && let Err(error) = cx
            .api()
            .delete_message(
                channel,
                Id::new(job.message),
                Reason::new(cx.ctx().t("delete-reason"))?,
            )
            .await
    {
        if error.is_retryable() {
            return Err(error.into());
        }
        if !matches!(error, DiscordError::NotFound) {
            tracing::warn!(?error, "could not delete message");
        }
    }
    Ok(())
}

fn should_delete(skip_pinned: bool, pinned: bool) -> bool {
    !skip_pinned || !pinned
}

#[cfg(test)]
mod tests {
    #[test]
    fn pinned_messages_are_only_deleted_when_allowed() {
        assert!(!super::should_delete(true, true));
        assert!(super::should_delete(false, true));
        assert!(super::should_delete(true, false));
    }
}
