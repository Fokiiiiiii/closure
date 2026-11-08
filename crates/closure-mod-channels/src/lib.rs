//! Automatic channel cleanup and reactions.
use closure_core::action::Emoji;
use closure_sdk::{
    Capability, CoreError, DiscordError, Event, EventContext, EventKind, Flow, Intents, JobCx,
    Locale, ModuleBuilder, ModuleError, Reason, SdkModule, Subscription, duration,
    parse_module_config,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;
use twilight_model::id::Id;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_field_names)]
struct Config {
    /// Whether the delete rules run. Off keeps them, and runs none.
    auto_delete_enabled: bool,
    auto_delete: Vec<DeleteRule>,
    /// Whether the reactions are added. Off keeps the rules, and adds none.
    auto_react_enabled: bool,
    auto_react: Vec<ReactRule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_delete_enabled: true,
            auto_delete: Vec::new(),
            auto_react_enabled: true,
            auto_react: Vec::new(),
        }
    }
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

const PROTECTED_WARNING_LIMIT: usize = 10_000;
static PROTECTED_WARNINGS: LazyLock<Mutex<HashSet<(u64, u64)>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn first_protected_warning(guild: u64, channel: u64) -> bool {
    let mut warnings = PROTECTED_WARNINGS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if warnings.contains(&(guild, channel)) || warnings.len() >= PROTECTED_WARNING_LIMIT {
        return false;
    }
    warnings.insert((guild, channel));
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
        "auto_delete_enabled": {"type": "boolean", "description": "config-auto-delete-enabled", "x-ui": {"level": "basic"}},
        "auto_react_enabled": {"type": "boolean", "description": "config-auto-react-enabled", "x-ui": {"level": "basic"}},
        "auto_delete": {"type": "array", "maxItems": 25, "description": "config-auto-delete", "x-ui": {"level": "basic"}, "items": {
            "type": "object", "additionalProperties": false, "required": ["channel", "after"],
            "properties": {"channel": channel, "after": {"type": "string", "pattern": "^[0-9]+[smhdw]( *[0-9]+[smhdw])*$"}, "skip_pinned": {"type": "boolean", "default": true}}
        }},
        "auto_react": {"type": "array", "maxItems": 25, "description": "config-auto-react", "x-ui": {"level": "basic"}, "items": {
            "type": "object", "additionalProperties": false, "required": ["channel", "emojis"],
            "properties": {"channel": channel, "emojis": {"type": "array", "minItems": 1, "maxItems": 5, "items": {"type": "string", "minLength": 1, "maxLength": 64}}, "skip_bots": {"type": "boolean", "default": true}}
        }}
    }});
    (
        schema,
        json!({"auto_delete_enabled": true, "auto_delete": [], "auto_react_enabled": true, "auto_react": []}),
    )
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = settings();
    let state = Arc::new(Sweeps::default());
    Ok(
        ModuleBuilder::new("channels", "Channels", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::ManageMessages)
            .capability(Capability::ReadMessages)
            .capability(Capability::AddReactions)
            .capability(Capability::Storage)
            .capability(Capability::Schedule)
            .intents(Intents::GUILD_MESSAGES)
            .on(Subscription::new(EventKind::MessageCreated), {
                let state = Arc::clone(&state);
                move |ctx, event| on_event(Arc::clone(&state), ctx, event)
            })
            .on(Subscription::new(EventKind::GuildAvailable), {
                let state = Arc::clone(&state);
                move |ctx, _| {
                    let state = Arc::clone(&state);
                    async move {
                        ensure_sweep(&state, &ctx).await?;
                        Ok(Flow::Continue)
                    }
                }
            })
            .on(Subscription::new(EventKind::ConfigChanged), {
                let state = Arc::clone(&state);
                move |ctx, _| {
                    let state = Arc::clone(&state);
                    async move {
                        ensure_sweep(&state, &ctx).await?;
                        Ok(Flow::Continue)
                    }
                }
            })
            .job(SWEEP, sweep)
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

async fn protected(ctx: &EventContext, channel: closure_core::event::ChannelId) -> bool {
    let mut channels = Vec::new();
    channels.extend(ctx.policy().log_channel);
    let logs = match closure_core::module::ModuleId::new("logs") {
        Ok(id) => ctx.module_config(&id),
        Err(_) => return true,
    };
    let mut names = Vec::new();
    if let Some(name) = ctx.policy().log_webhook.as_deref() {
        names.push(name.to_owned());
    }
    if let Some(config) = logs {
        for (key, webhook) in [("channel", false), ("webhook", true)] {
            match config.get(key) {
                None | Some(serde_json::Value::Null) => {}
                Some(serde_json::Value::String(value)) if webhook && !value.is_empty() => {
                    names.push(value.clone());
                }
                Some(serde_json::Value::String(value)) if !webhook => {
                    let Some(id) = value.parse::<u64>().ok().and_then(Id::new_checked) else {
                        return true;
                    };
                    channels.push(id);
                }
                _ => return true,
            }
        }
    }
    for name in names {
        match ctx.api.webhook_channel(&name).await {
            Ok(id) => channels.push(id),
            Err(_) => return true,
        }
    }
    channels.contains(&channel)
}

async fn on_event(
    state: Arc<Sweeps>,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = event else {
        return Ok(Flow::Continue);
    };
    let config: Config = parse_module_config(&ctx)?;
    config.validate()?;
    let channel = message.channel_id.to_string();
    let (deletes_on, reacts_on) = (config.auto_delete_enabled, config.auto_react_enabled);
    let has_rule = (deletes_on
        && config
            .auto_delete
            .iter()
            .any(|rule| rule.channel == channel))
        || (reacts_on && config.auto_react.iter().any(|rule| rule.channel == channel));
    if has_rule && protected(&ctx, message.channel_id).await {
        if first_protected_warning(ctx.guild_id.get(), message.channel_id.get()) {
            tracing::warn!("ignoring channel rule for protected or unresolved log destination");
        } else {
            tracing::debug!("ignoring channel rule for protected or unresolved log destination");
        }
        return Ok(Flow::Continue);
    }
    let mut deletes = config
        .auto_delete
        .iter()
        .filter(|rule| deletes_on && rule.channel == channel);
    if let Some(rule) = deletes.next() {
        if deletes.next().is_some() {
            tracing::warn!("duplicate auto_delete channel; using first rule");
        }
        let delay = duration::parse(&rule.after).map_err(|e| ModuleError::Config(e.to_string()))?;
        // Kept in a set for the minute it falls due in, not as a job of its own: a busy channel
        // would use up the jobs of the whole guild.
        let due = ctx
            .now()
            .saturating_add(i64::try_from(delay.as_secs()).unwrap_or(i64::MAX));
        ctx.store()
            .set_add(
                &bucket_set(message.channel_id.get(), bucket_of(due)),
                message.id.get(),
            )
            .await
            .map_err(|e| ModuleError::other(e.to_string()))?;
        start_cursor(&state, &ctx, message.channel_id.get(), bucket_of(due)).await?;
        ensure_sweep(&state, &ctx).await?;
    }
    let mut reactions = config
        .auto_react
        .iter()
        .filter(|rule| reacts_on && rule.channel == channel);
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

/// The job that deletes the messages that are due, and how often it runs.
const SWEEP: &str = "sweep";
const SWEEP_EVERY: Duration = Duration::from_secs(10);
/// Messages that fall due within the same few seconds share a set.
const BUCKET_SECONDS: i64 = 10;
/// How many messages one run deletes at most, and how many empty sets it steps over.
const SWEEP_BUDGET: usize = 100;
const SWEEP_STEPS: i64 = 720;

#[derive(Default)]
struct Sweeps {
    ensured: Mutex<HashSet<u64>>,
    started: Mutex<HashSet<u64>>,
}

/// Puts the cursor of a channel's sweep just before its first set when there is none yet, so that
/// a sweep that comes late (or after a restart) starts from the oldest message that waits and not
/// from now.
async fn start_cursor(
    state: &Sweeps,
    ctx: &EventContext,
    channel: u64,
    bucket: i64,
) -> Result<(), ModuleError> {
    if state
        .started
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&channel)
    {
        return Ok(());
    }
    let store = ctx.store();
    let held: Option<i64> = store
        .get(&cursor_key(channel))
        .await
        .map_err(|e| ModuleError::other(e.to_string()))?;
    if held.is_none() {
        store
            .set(&cursor_key(channel), &(bucket - 1))
            .await
            .map_err(|e| ModuleError::other(e.to_string()))?;
    }
    state
        .started
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(channel);
    Ok(())
}

/// The number of the set a deletion at `due` (Unix seconds) belongs to: the first whose end is not
/// before it.
fn bucket_of(due: i64) -> i64 {
    due.div_euclid(BUCKET_SECONDS) + i64::from(due.rem_euclid(BUCKET_SECONDS) != 0)
}

fn bucket_set(channel: u64, bucket: i64) -> String {
    format!("del-{channel}-{bucket}")
}

fn cursor_key(channel: u64) -> String {
    format!("del-cursor:{channel}")
}

/// Makes sure the guild has its sweep job, once for each run of the bot.
async fn ensure_sweep(state: &Sweeps, ctx: &EventContext) -> Result<(), ModuleError> {
    if state
        .ensured
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&ctx.guild_id.get())
    {
        return Ok(());
    }
    ctx.scheduler()
        .job(SWEEP, &())
        .map_err(|e| ModuleError::other(e.to_string()))?
        .key(SWEEP)
        .if_absent()
        .every(SWEEP_EVERY, Duration::ZERO)
        .await
        .map_err(|e| ModuleError::other(e.to_string()))?;
    state
        .ensured
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(ctx.guild_id.get());
    Ok(())
}

/// Deletes the messages of the sets that are due, channel by channel, with a budget for the run.
/// A deletion that fails for a reason that passes leaves its message in the set and ends the run:
/// the next one tries again.
async fn sweep(cx: JobCx) -> Result<(), ModuleError> {
    let ctx = cx.ctx();
    let config: Config = parse_module_config(ctx)?;
    if !config.auto_delete_enabled {
        return Ok(());
    }
    let now_bucket = ctx.now().div_euclid(BUCKET_SECONDS);
    // Each channel has its share of the run, so the first of the list cannot use all of it.
    let share = SWEEP_BUDGET
        .div_ceil(config.auto_delete.len().max(1))
        .max(1);
    for rule in &config.auto_delete {
        let mut budget = share;
        let Ok(channel) = rule.channel.parse::<u64>() else {
            continue;
        };
        if protected(ctx, Id::new(channel)).await {
            continue;
        }
        let store = ctx.store();
        let mut cursor: i64 = store
            .get(&cursor_key(channel))
            .await
            .map_err(|e| ModuleError::other(e.to_string()))?
            .unwrap_or(now_bucket - 1);
        let mut steps = 0;
        while cursor < now_bucket && steps < SWEEP_STEPS && budget > 0 {
            let bucket = cursor + 1;
            let name = bucket_set(channel, bucket);
            // A message leaves its set once it is deleted, so a stop in the middle loses none.
            let mut after = 0_u64;
            loop {
                let batch = store
                    .set_after(&name, after, budget.min(SWEEP_BUDGET))
                    .await
                    .map_err(|e| ModuleError::other(e.to_string()))?;
                let Some(last) = batch.last().copied() else {
                    break;
                };
                after = last;
                for message in batch {
                    match delete_message(ctx, Id::new(channel), message, rule.skip_pinned).await {
                        Ok(()) => {
                            store
                                .set_remove(&name, message)
                                .await
                                .map_err(|e| ModuleError::other(e.to_string()))?;
                            budget = budget.saturating_sub(1);
                        }
                        Err(error) => {
                            tracing::warn!(?error, "could not delete a message that is due; later");
                            return Ok(());
                        }
                    }
                }
                if budget == 0 {
                    break;
                }
            }
            let left = store
                .set_count(&name)
                .await
                .map_err(|e| ModuleError::other(e.to_string()))?;
            if left > 0 {
                break;
            }
            cursor = bucket;
            steps += 1;
            store
                .set(&cursor_key(channel), &cursor)
                .await
                .map_err(|e| ModuleError::other(e.to_string()))?;
        }
    }
    Ok(())
}

/// A job of the first format, one for a message: still deleted, if one is left from before.
async fn delete(cx: JobCx) -> Result<(), ModuleError> {
    let job: DeleteJob = cx.payload()?;
    let channel = Id::new(job.channel);
    if protected(cx.ctx(), channel).await {
        if first_protected_warning(cx.ctx().guild_id.get(), channel.get()) {
            tracing::warn!("ignoring auto_delete for protected or unresolved log destination");
        } else {
            tracing::debug!("ignoring auto_delete for protected or unresolved log destination");
        }
        return Ok(());
    }
    delete_message(cx.ctx(), channel, job.message, job.skip_pinned).await
}

/// Deletes a message unless it is pinned and pins are kept. A message that is gone is done; a
/// failure that passes is an error to try again, any other is only logged.
async fn delete_message(
    ctx: &EventContext,
    channel: closure_core::event::ChannelId,
    message_id: u64,
    skip_pinned: bool,
) -> Result<(), ModuleError> {
    let message = match ctx.api.get_message(channel, Id::new(message_id)).await {
        Ok(message) => message,
        Err(DiscordError::NotFound) => return Ok(()),
        Err(error) if error.is_retryable() => return Err(error.into()),
        Err(error) => {
            tracing::warn!(?error, "could not retrieve message for deletion");
            return Ok(());
        }
    };
    if should_delete(skip_pinned, message.pinned)
        && let Err(error) = ctx
            .api
            .delete_message(
                channel,
                Id::new(message_id),
                Reason::new(ctx.t("delete-reason"))?,
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

    #[test]
    fn protected_channel_warning_is_emitted_once_per_guild_and_channel() {
        assert!(super::first_protected_warning(u64::MAX, u64::MAX));
        assert!(!super::first_protected_warning(u64::MAX, u64::MAX));
        assert!(super::first_protected_warning(u64::MAX - 1, u64::MAX));
    }
}
