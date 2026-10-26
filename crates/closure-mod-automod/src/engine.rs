//! The pipeline of automod. A message goes through the enabled rules in the order of their ids. A
//! rule that trips acts; the rules after it are not looked at, unless it says they should be.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use closure_core::error::DiscordError;
use closure_core::permission::Actor;
use closure_sdk::recent::{RecentActions, RecentLimits};
use closure_sdk::{
    Event, EventContext, Flow, GuildId, Level, MessageInfo, ModuleError, parse_module_config,
};
use serde_json::Value;

use crate::actions;
use crate::config::{Config, Entry, Rule, Scope};
use crate::spam;
use crate::triggers::{self, Hit, Scan};

/// What the module keeps between messages: the recent actions of every guild, and the rules of
/// each guild read once rather than on every message (a regex is compiled when it is read).
#[derive(Debug)]
pub struct State {
    recent: RecentActions,
    loaded: Mutex<HashMap<GuildId, Arc<Loaded>>>,
}

/// A guild's configuration as it was read, and the rules of it.
#[derive(Debug)]
pub struct Loaded {
    /// The configuration it was read from; the reading is kept while this stays the same.
    source: Value,
    config: Config,
    rules: Vec<Entry>,
}

impl Loaded {
    fn valid(&self) -> impl Iterator<Item = &Rule> {
        self.rules.iter().filter_map(|entry| match entry {
            Entry::Valid(rule) => Some(rule),
            Entry::Invalid { .. } => None,
        })
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            recent: RecentActions::new(RecentLimits::default()),
            loaded: Mutex::new(HashMap::new()),
        }
    }

    fn load(&self, ctx: &EventContext) -> Result<Arc<Loaded>, ModuleError> {
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = loaded.get(&ctx.guild_id)
            && found.source == *ctx.config()
        {
            return Ok(Arc::clone(found));
        }
        let config: Config = parse_module_config(ctx)?;
        let rules = crate::config::parse_rules(&config.rules);
        let fresh = Arc::new(Loaded {
            source: ctx.config().clone(),
            config,
            rules,
        });
        loaded.insert(ctx.guild_id, Arc::clone(&fresh));
        Ok(fresh)
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// Handles a message that was posted or edited.
pub async fn on_message(
    state: &State,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    let (message, created) = match &event {
        Event::MessageCreated(message) => (message, true),
        Event::MessageUpdated(message) => (message, false),
        _ => return Ok(Flow::Continue),
    };
    // The bot's own messages and the owner's are never checked, whatever the rules say.
    if ctx.bot() == Some(message.author.id)
        || ctx.api.guild_owner().await? == Some(message.author.id)
    {
        return Ok(Flow::Continue);
    }
    // An edit with neither text nor files (a link preview, say) has nothing to check.
    if !created && message.content.is_empty() && message.attachments.is_empty() {
        return Ok(Flow::Continue);
    }
    let loaded = state.load(&ctx)?;
    let now = ctx.now();
    let mut joined = Joined::default();
    let mut scoped = Vec::new();
    for rule in loaded.valid().filter(|rule| rule.enabled) {
        if in_scope(&ctx, &rule.scope, message, &mut joined).await? {
            scoped.push(rule);
        }
    }
    let scan = Scan {
        ctx: &ctx,
        recent: &state.recent,
        commit: !loaded.config.dry_run,
        created,
    };
    // Counted before any rule is checked, so that the message which trips a burst is one of it.
    if created {
        for rule in &scoped {
            spam::record(&scan, rule, message, now);
        }
    }
    for rule in scoped {
        if on_cooldown(&ctx, rule, message).await? {
            continue;
        }
        let Some(hit) = first_hit(&scan, rule, message) else {
            continue;
        };
        act(&ctx, rule, &hit, message, &loaded.config).await?;
        if !loaded.config.dry_run && !hit.join {
            start_cooldown(&ctx, rule, message).await?;
        }
        if !rule.continue_after_match {
            break;
        }
    }
    Ok(Flow::Continue)
}

/// The first trigger of the rule that finds something, with what it found.
fn first_hit(scan: &Scan<'_>, rule: &Rule, message: &MessageInfo) -> Option<Hit> {
    (0..rule.triggers.len()).find_map(|index| triggers::check(scan, rule, index, message))
}

async fn act(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    message: &MessageInfo,
    config: &Config,
) -> Result<(), ModuleError> {
    if config.dry_run {
        return actions::report(ctx, rule, hit, message).await;
    }
    actions::apply(ctx, rule, hit, message, config).await
}

/// Whether the rule applies to the message at all: its channels, the author's roles, bots,
/// moderators unless the rule says otherwise, and the member's age when the rule asks for it.
async fn in_scope(
    ctx: &EventContext,
    scope: &Scope,
    message: &MessageInfo,
    joined: &mut Joined,
) -> Result<bool, ModuleError> {
    let roles = &message.author_roles;
    if !scope.channels.is_empty() && !scope.channels.contains(&message.channel_id) {
        return Ok(false);
    }
    if scope.exclude_channels.contains(&message.channel_id) {
        return Ok(false);
    }
    if !scope.roles.is_empty() && !roles.iter().any(|role| scope.roles.contains(role)) {
        return Ok(false);
    }
    if roles.iter().any(|role| scope.exclude_roles.contains(role)) {
        return Ok(false);
    }
    if message.author.bot && !scope.include_bots {
        return Ok(false);
    }
    if scope.exempt_moderators && is_moderator(ctx, message) {
        return Ok(false);
    }
    let age = scope.member_age;
    if age.min_days.is_none() && age.max_days.is_none() {
        return Ok(true);
    }
    let Some(joined_at) = joined_at(ctx, message, joined).await? else {
        return Ok(false);
    };
    let days = (ctx.now() - joined_at).div_euclid(86_400);
    let old_enough = age.min_days.is_none_or(|min| days >= i64::from(min));
    let young_enough = age.max_days.is_none_or(|max| days <= i64::from(max));
    Ok(old_enough && young_enough)
}

/// When the member joined, fetched once per message. `None` when Discord does not know it.
#[derive(Debug, Default)]
pub struct Joined {
    checked: bool,
    at: Option<i64>,
}

/// When the member joined, fetched once per message; `None` when Discord does not know it.
async fn joined_at(
    ctx: &EventContext,
    message: &MessageInfo,
    memo: &mut Joined,
) -> Result<Option<i64>, ModuleError> {
    if memo.checked {
        return Ok(memo.at);
    }
    let found = match ctx.api.get_member(ctx.guild_id, message.author.id).await {
        Ok(member) => member
            .joined_at
            .map(twilight_model::util::Timestamp::as_secs),
        Err(DiscordError::NotFound) => None,
        Err(error) => return Err(error.into()),
    };
    *memo = Joined {
        checked: true,
        at: found,
    };
    Ok(found)
}

/// Whether the author reaches the moderator level through the same permissions that the commands
/// use. Administrators are not known from a message, so they count as members here.
fn is_moderator(ctx: &EventContext, message: &MessageInfo) -> bool {
    let actor = Actor {
        user: message.author.id,
        roles: message.author_roles.clone(),
        administrator: false,
        owner: false,
    };
    ctx.permissions().level_of(&actor) >= Level::MODERATOR
}

fn cooldown_key(rule: &Rule, message: &MessageInfo) -> String {
    format!("cooldown:{}:{}", rule.id, message.author.id.get())
}

async fn on_cooldown(
    ctx: &EventContext,
    rule: &Rule,
    message: &MessageInfo,
) -> Result<bool, ModuleError> {
    if rule.cooldown.is_none() {
        return Ok(false);
    }
    let found = ctx
        .store()
        .get::<bool>(&cooldown_key(rule, message))
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(found.is_some())
}

async fn start_cooldown(
    ctx: &EventContext,
    rule: &Rule,
    message: &MessageInfo,
) -> Result<(), ModuleError> {
    let Some(cooldown) = rule.cooldown else {
        return Ok(());
    };
    ctx.store()
        .set_for(&cooldown_key(rule, message), &true, cooldown)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))
}
