//! The pipeline of automod. A message goes through the enabled rules in order; the first one that
//! hits acts, and the rest are not looked at.

use closure_core::permission::Actor;
use closure_sdk::{
    Event, EventContext, Flow, Level, MessageInfo, ModuleError, parse_module_config,
};

use crate::actions;
use crate::checks::{self, Hit};
use crate::config::{Check, Config, Entry, Rule, Scope, parse_rules};

/// Handles a message that was posted or edited.
pub async fn on_message(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
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
    let config: Config = parse_module_config(&ctx)?;
    for entry in parse_rules(&config.rules) {
        // Invalid rules never run; `/automod status` lists them.
        let Entry::Valid(rule) = entry else {
            continue;
        };
        if !rule.enabled || !in_scope(&ctx, &rule.scope, message) {
            continue;
        }
        let Some(hit) = checks::evaluate(&ctx, &rule, message, created).await? else {
            continue;
        };
        act(&ctx, &rule, &hit, message, &config).await?;
        break;
    }
    Ok(Flow::Continue)
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
    let result = actions::apply(ctx, rule, hit, message, config).await;
    // A flood starts counting again after a hit, so one burst fires once. This happens even when
    // an action failed, so that the next message is not counted against the old burst.
    let reset = if matches!(rule.check, Check::Flood { .. }) {
        ctx.store()
            .counter_reset(&rule.id, message.author.id.get())
            .await
            .map_err(|error| ModuleError::other(error.to_string()))
    } else {
        Ok(())
    };
    result.and(reset)
}

/// Whether the rule applies to the message at all: its channels, the author's roles, bots, and
/// moderators unless the rule says otherwise.
fn in_scope(ctx: &EventContext, scope: &Scope, message: &MessageInfo) -> bool {
    let roles = &message.author_roles;
    if !scope.channels.is_empty() && !scope.channels.contains(&message.channel_id) {
        return false;
    }
    if scope.exclude_channels.contains(&message.channel_id) {
        return false;
    }
    if !scope.roles.is_empty() && !roles.iter().any(|role| scope.roles.contains(role)) {
        return false;
    }
    if roles.iter().any(|role| scope.exclude_roles.contains(role)) {
        return false;
    }
    if message.author.bot && !scope.include_bots {
        return false;
    }
    !(scope.exempt_moderators && is_moderator(ctx, message))
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
