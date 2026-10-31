//! What a rule does once it hits: its actions in order, the record they leave, and the report of a
//! dry run. An action runs for each member a hit names, and an action that needs a message or a
//! channel that the context does not have is skipped without a word.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

use closure_core::action::{Action as Discord, ChannelPatch, MentionPolicy, Reason};
use closure_core::error::DiscordError;
use closure_core::event::MessageId;
use closure_sdk::counters::CounterDef;
use closure_sdk::embed::{Color, EmbedBuilder, truncate};
use closure_sdk::modlog;
use closure_sdk::punish::{
    Done, Method, Moderator, Punisher, RestrictionRole, Settings, Strip, ensure_role,
};
use closure_sdk::template::{Template, Values};
use closure_sdk::{
    Args, CaseKind, ChannelId, EventContext, JobCx, MessageInfo, ModuleError, NewCase, RoleId,
    UserError, UserId,
};
use serde::{Deserialize, Serialize};
use twilight_model::channel::message::Embed;
use twilight_model::id::Id;

use crate::config::{Action, Config, Rule, action_key};
use crate::context::{Context, Source};
use crate::counter::{self, Op};
use crate::memory::Memory;
use crate::triggers::Hit;

const DAY_SECONDS: u32 = 86_400;
/// The name the bot's own punishments and records carry.
const BOT_NAME: &str = "automod";
/// Discord takes at most this many messages in one bulk delete.
const BULK_DELETE: usize = 100;
/// Discord's longest message.
const CONTENT_CHARS: usize = 2_000;
/// The job that removes a reply once its `auto_delete` time is up.
pub const CLEANUP_JOB: &str = "automod_cleanup";

/// What the actions of a rule are given besides the rule, the hit and the context.
pub struct Deps<'a> {
    pub ctx: &'a EventContext,
    pub memory: &'a Memory,
    pub counters: &'a [CounterDef],
    pub config: &'a Config,
    pub antiraid: Option<&'a crate::antiraid::Antiraid>,
}

/// Whether an action ran, and whether it punished a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ran {
    /// Nothing to act on in this context, so nothing was done.
    Skipped,
    Other,
    Punished,
}

/// What the actions of a rule left behind. The counter changes they made are stored, so their
/// triggers are returned whatever happened after them; the first error is returned beside them.
#[derive(Debug, Default)]
pub struct Applied {
    pub fired: Vec<Context>,
    pub error: Option<ModuleError>,
}

/// Runs the rule's actions in order, for each member of the hit. A failing one does not stop the
/// next; the first error is kept. When nothing punished a member, the hit is recorded as an `automod`
/// case for each of them, so that a deletion still shows in the log. A later message of a burst only
/// has its messages deleted. The counter triggers that the actions fired are returned, for the engine
/// to run.
pub async fn apply(deps: &Deps<'_>, rule: &Rule, hit: &Hit, context: &Context) -> Applied {
    let ctx = deps.ctx;
    let mut applied = Applied::default();
    let bot = match Moderator::bot(ctx, BOT_NAME) {
        Ok(bot) => bot,
        Err(error) => {
            applied.error = Some(error);
            return applied;
        }
    };
    let settings = match punish_settings(ctx, deps.config, rule).await {
        Ok(settings) => settings,
        Err(error) => {
            applied.error = Some(error);
            return applied;
        }
    };
    let punisher = Punisher::new(ctx, bot.clone(), settings);
    let mut ran = false;
    let mut did_punish = false;
    for action in &rule.actions {
        if hit.join && !matches!(action, Action::Delete(_)) {
            continue;
        }
        match run(deps, &punisher, rule, hit, context, action).await {
            Ok((Ran::Skipped, _)) => {}
            Ok((outcome, more)) => {
                ran = true;
                did_punish |= outcome == Ran::Punished;
                applied.fired.extend(more);
            }
            Err(error) => {
                if !suppressed(deps, rule, &error) {
                    applied.error.get_or_insert(error);
                }
            }
        }
    }
    if ran && !did_punish && !hit.join {
        for target in &hit.targets {
            let reason = reason_for(ctx, rule, hit, context, Some(*target), None);
            if let Err(error) = record(ctx, &bot, *target, &reason).await {
                applied.error.get_or_insert(error);
            }
        }
    }
    applied
}

/// A missing permission is reported once a minute per rule; the other times it is not an error.
fn suppressed(deps: &Deps<'_>, rule: &Rule, error: &ModuleError) -> bool {
    let forbidden = matches!(error, ModuleError::Discord(DiscordError::Forbidden(_)));
    forbidden
        && !deps
            .memory
            .should_report(deps.ctx.guild_id, &rule.id, deps.ctx.now())
}

/// A dry run: posts what the rule would have done to the log channel, and changes nothing. Without
/// a log channel there is nowhere to say it, so nothing happens.
pub async fn report(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
) -> Result<(), ModuleError> {
    let Some(channel) = ctx.policy().log_channel else {
        return Ok(());
    };
    let would_run = rule
        .actions
        .iter()
        .map(|action| text(ctx, action_key(action), &Args::new()))
        .collect::<Vec<_>>()
        .join(", ");
    let member = hit
        .targets
        .first()
        .copied()
        .or_else(|| context.user_id())
        .map_or_else(String::new, mention);
    let embed = EmbedBuilder::new()
        .title(text(ctx, "dry-run-title", &Args::new()))
        .color(Color::Info)
        .field(
            text(ctx, "field-rule", &Args::new()),
            rule.name.as_str(),
            true,
        )
        .field(text(ctx, "field-member", &Args::new()), member, true)
        .field(
            text(ctx, "field-check", &Args::new()),
            hit.detail.as_str(),
            false,
        )
        .field(text(ctx, "field-would-run", &Args::new()), would_run, false)
        .build()
        .map_err(|error| ModuleError::other(error.to_string()))?;
    send(ctx, channel, embed).await
}

/// Runs one action. The result says whether it ran and punished, and what counter triggers it fired.
async fn run(
    deps: &Deps<'_>,
    punisher: &Punisher<'_>,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    action: &Action,
) -> Result<(Ran, Vec<Context>), ModuleError> {
    let ctx = deps.ctx;
    let targets = &hit.targets;
    match action {
        Action::Delete(_) => delete_hit(deps, rule, hit, context).await,
        Action::Purge { within } => {
            if targets.is_empty() {
                return Ok(skipped());
            }
            let reason = reason_for(ctx, rule, hit, context, targets.first().copied(), None);
            purge(ctx, hit, targets, *within, &reason)
                .await
                .map(|()| other())
        }
        Action::Warn { .. } | Action::Timeout { .. } | Action::Kick { .. } | Action::Ban { .. } => {
            punish(deps, punisher, rule, hit, context, action).await
        }
        Action::Reply { text, auto_delete } => match context.message.as_ref() {
            Some(message) => {
                let values = values(ctx, rule, hit, context, None);
                reply(ctx, message, text, *auto_delete, &values)
                    .await
                    .map(|()| other())
            }
            None => Ok(skipped()),
        },
        Action::Alert { channel, text } => {
            alert(ctx, rule, hit, context, *channel, text.as_deref())
                .await
                .map(|()| other())
        }
        Action::AddRoles { roles } => roles_action(deps, rule, hit, context, roles, true).await,
        Action::RemoveRoles { roles } => roles_action(deps, rule, hit, context, roles, false).await,
        Action::SetNickname { name } => nickname(deps, rule, hit, context, name).await,
        Action::Slowmode { duration, channels } => {
            slowmode(deps, rule, hit, context, *duration, channels).await
        }
        Action::AddToCounter { counter, amount } => {
            change_counter(deps, context, counter, Op::Add(*amount)).await
        }
        Action::SetAntiraidLevel { level, duration } => {
            set_antiraid_level(deps, level, *duration).await
        }
        Action::SetCounter { counter, value } => {
            change_counter(deps, context, counter, Op::Set(*value)).await
        }
    }
}

fn skipped() -> (Ran, Vec<Context>) {
    (Ran::Skipped, Vec::new())
}

fn other() -> (Ran, Vec<Context>) {
    (Ran::Other, Vec::new())
}

/// Deletes the messages of the hit, when there are any.
async fn delete_hit(
    deps: &Deps<'_>,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
) -> Result<(Ran, Vec<Context>), ModuleError> {
    if hit.messages.is_empty() {
        return Ok(skipped());
    }
    let reason = reason_for(
        deps.ctx,
        rule,
        hit,
        context,
        hit.targets.first().copied(),
        None,
    );
    delete_all(deps.ctx, &hit.messages, &reason)
        .await
        .map(|()| other())
}

/// Warns, times out, kicks or bans each member of the hit.
async fn punish(
    deps: &Deps<'_>,
    punisher: &Punisher<'_>,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    action: &Action,
) -> Result<(Ran, Vec<Context>), ModuleError> {
    let ctx = deps.ctx;
    let targets = &hit.targets;
    if targets.is_empty() {
        return Ok(skipped());
    }
    let reason = |target: UserId, template: Option<&str>| {
        reason_for(ctx, rule, hit, context, Some(target), template)
    };
    let result = match action {
        Action::Warn { reason: template } => {
            let expires_at = ctx.now() + i64::from(deps.config.warn_expire_days) * 86_400;
            each(targets, |target| {
                let text = reason(target, template.as_deref());
                async move { punished(punisher.warn(target, Some(text), Some(expires_at)).await) }
            })
            .await
        }
        Action::Timeout {
            duration,
            reason: template,
        } => {
            each(targets, |target| {
                let text = reason(target, template.as_deref());
                async move { punished(punisher.timeout(target, *duration, Some(text)).await) }
            })
            .await
        }
        Action::Kick { reason: template } => {
            each(targets, |target| {
                let text = reason(target, template.as_deref());
                async move { punished(punisher.kick(target, Some(text)).await) }
            })
            .await
        }
        Action::Ban {
            duration,
            delete_days,
            reason: template,
        } => {
            let seconds = delete_days.saturating_mul(DAY_SECONDS);
            each(targets, |target| {
                let text = reason(target, template.as_deref());
                async move { punished(punisher.ban(target, *duration, seconds, Some(text)).await) }
            })
            .await
        }
        // Only the four punishments are sent here, by `run`.
        _ => Ok(()),
    };
    result.map(|()| (Ran::Punished, Vec::new()))
}

/// Gives or takes the roles from each member of the hit.
async fn roles_action(
    deps: &Deps<'_>,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    roles: &[RoleId],
    add: bool,
) -> Result<(Ran, Vec<Context>), ModuleError> {
    if hit.targets.is_empty() {
        return Ok(skipped());
    }
    each(&hit.targets, |target| {
        let reason = reason_for(deps.ctx, rule, hit, context, Some(target), None);
        async move { change_roles(deps, target, roles, add, &reason).await }
    })
    .await
    .map(|()| other())
}

/// Sets the nickname of each member of the hit, from a template.
async fn nickname(
    deps: &Deps<'_>,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    name: &str,
) -> Result<(Ran, Vec<Context>), ModuleError> {
    let ctx = deps.ctx;
    if hit.targets.is_empty() {
        return Ok(skipped());
    }
    each(&hit.targets, |target| {
        let nickname = render(name, &values(ctx, rule, hit, context, Some(target)));
        let reason = reason_for(ctx, rule, hit, context, Some(target), None);
        async move {
            ctx.api
                .execute(Discord::SetNickname {
                    guild_id: ctx.guild_id,
                    user_id: target,
                    nickname: Some(nickname),
                    reason: Reason::truncated(&reason),
                })
                .await
                .map(drop)
                .map_err(ModuleError::from)
        }
    })
    .await
    .map(|()| other())
}

/// Sets the slowmode of the channels given, or of the channel of the event.
async fn slowmode(
    deps: &Deps<'_>,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    duration: Duration,
    channels: &[ChannelId],
) -> Result<(Ran, Vec<Context>), ModuleError> {
    let channels: Vec<ChannelId> = if channels.is_empty() {
        context.channel.into_iter().collect()
    } else {
        channels.to_vec()
    };
    if channels.is_empty() {
        return Ok(skipped());
    }
    let seconds = u16::try_from(duration.as_secs()).unwrap_or(u16::MAX);
    let reason = reason_for(
        deps.ctx,
        rule,
        hit,
        context,
        hit.targets.first().copied(),
        None,
    );
    set_slowmode(deps, &channels, seconds, &reason)
        .await
        .map(|()| other())
}

/// Runs `act` for each member, and keeps the first error: one member failing does not stop the others.
async fn each<F, Fut>(targets: &[UserId], mut act: F) -> Result<(), ModuleError>
where
    F: FnMut(UserId) -> Fut,
    Fut: Future<Output = Result<(), ModuleError>>,
{
    let mut first_error = None;
    for target in targets.iter().copied() {
        if let Err(error) = act(target).await {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Gives or takes each role of the list from the member. The bot's own change is remembered first,
/// so that the role event Discord sends for it is not counted as someone else's.
async fn change_roles(
    deps: &Deps<'_>,
    target: UserId,
    roles: &[RoleId],
    add: bool,
    reason: &str,
) -> Result<(), ModuleError> {
    let ctx = deps.ctx;
    let mut first_error = None;
    for role in roles {
        deps.memory
            .ignore_role(ctx.guild_id, target, *role, ctx.now());
        let result = if add {
            ctx.api
                .add_role(ctx.guild_id, target, *role, Reason::truncated(reason))
                .await
        } else {
            ctx.api
                .remove_role(ctx.guild_id, target, *role, Reason::truncated(reason))
                .await
        };
        if let Err(error) = result {
            first_error.get_or_insert(ModuleError::from(error));
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Sets the slowmode of each channel. A slowmode that was just set to the same value is not set
/// again, since Discord limits how often a channel is edited.
async fn set_slowmode(
    deps: &Deps<'_>,
    channels: &[ChannelId],
    seconds: u16,
    reason: &str,
) -> Result<(), ModuleError> {
    let ctx = deps.ctx;
    let mut first_error = None;
    for channel in channels {
        if deps.memory.is_repeat_slowmode(*channel, seconds, ctx.now()) {
            continue;
        }
        let patch = ChannelPatch {
            slowmode_seconds: Some(seconds),
            ..ChannelPatch::default()
        };
        let result = ctx
            .api
            .execute(Discord::EditChannel {
                channel_id: *channel,
                patch,
                reason: Reason::truncated(reason),
            })
            .await;
        match result {
            Ok(_) => deps.memory.remember_slowmode(*channel, seconds, ctx.now()),
            Err(error) => {
                first_error.get_or_insert(ModuleError::from(error));
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Raises or lowers the antiraid level. The change is a context of its own, so the rules that watch
/// the level see it, a few levels deep at most.
async fn set_antiraid_level(
    deps: &Deps<'_>,
    level: &str,
    duration: Option<Duration>,
) -> Result<(Ran, Vec<Context>), ModuleError> {
    let Some(antiraid) = deps.antiraid else {
        return Err(ModuleError::other("antiraid is not set up"));
    };
    let change = crate::antiraid::set_level(
        deps.ctx,
        antiraid,
        level,
        crate::antiraid::Setter::Bot,
        duration,
        None,
        deps.config.dry_run,
    )
    .await?;
    Ok(match change {
        // The effects of the level that is set were made again: the rules see no change.
        Some(change) if change.reapplied() => (Ran::Other, Vec::new()),
        Some(change) => (
            Ran::Other,
            vec![Context::antiraid(&change.new, &change.old)],
        ),
        None => skipped(),
    })
}

/// Changes a counter for the member and the channel of the context. The triggers the change fired
/// come back as contexts of their own.
async fn change_counter(
    deps: &Deps<'_>,
    context: &Context,
    name: &str,
    op: Op,
) -> Result<(Ran, Vec<Context>), ModuleError> {
    let Some(def) = counter::find(deps.counters, name) else {
        return Err(ModuleError::other(format!(
            "there is no counter named `{name}`"
        )));
    };
    let who = context.user.as_ref();
    let user = who.map(|who| who.id);
    let channel = context.channel;
    if let Err(missing) = counter::check_scope(def, channel, user) {
        return Err(ModuleError::other(format!(
            "counter `{name}` is kept per {}, and this event has none",
            match missing {
                counter::Missing::Channel => "channel",
                counter::Missing::User => "user",
            }
        )));
    }
    let change = counter::change(deps.ctx, deps.counters, name, channel, user, op).await?;
    let fired = counter::fired(&change, name, channel, who);
    Ok((Ran::Other, fired))
}

/// The result of a punishment as an action's, with the notes of it left out.
fn punished(result: Result<Done, UserError>) -> Result<(), ModuleError> {
    result.map(drop).map_err(module_error)
}

fn module_error(error: UserError) -> ModuleError {
    match error {
        UserError::Module(error) => error,
        UserError::Message { key, .. } | UserError::Sdk { key, .. } => ModuleError::other(key),
        other => ModuleError::other(format!("{other:?}")),
    }
}

/// Deletes what the member sent within `within`, from the history, and the messages of the hit.
async fn purge(
    ctx: &EventContext,
    hit: &Hit,
    targets: &[UserId],
    within: Duration,
    reason: &str,
) -> Result<(), ModuleError> {
    let since = ctx.now() - i64::try_from(within.as_secs()).unwrap_or(i64::MAX);
    let mut messages = hit.messages.clone();
    for target in targets {
        let saved = ctx
            .history()
            .by_author(*target, since, BULK_DELETE)
            .await
            .map_err(|error| ModuleError::other(format!("purge: {error}")))?;
        messages.extend(
            saved
                .iter()
                .filter(|message| message.deleted_at.is_none())
                .map(|message| (message.channel_id, message.id)),
        );
    }
    delete_all(ctx, &messages, reason).await
}

/// Deletes the messages: one bulk delete for each channel that has two or more of them (in
/// batches of a hundred), a single delete for a lone one. A message that is gone already is not an
/// error.
async fn delete_all(
    ctx: &EventContext,
    messages: &[(ChannelId, MessageId)],
    reason: &str,
) -> Result<(), ModuleError> {
    let mut by_channel: BTreeMap<ChannelId, Vec<MessageId>> = BTreeMap::new();
    for (channel, message) in messages {
        by_channel.entry(*channel).or_default().push(*message);
    }
    let mut first_error = None;
    for (channel, mut ids) in by_channel {
        ids.sort();
        ids.dedup();
        for batch in ids.chunks(BULK_DELETE) {
            let result = if let [single] = batch {
                ctx.api
                    .delete_message(channel, *single, Reason::truncated(reason))
                    .await
            } else {
                ctx.api
                    .bulk_delete_messages(channel, batch.to_vec(), Reason::truncated(reason))
                    .await
            };
            match result {
                Ok(()) | Err(DiscordError::NotFound) => {}
                Err(error) => {
                    first_error.get_or_insert(ModuleError::from(error));
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Sends the reply in the channel of the message, optionally removing it again later.
async fn reply(
    ctx: &EventContext,
    message: &MessageInfo,
    template: &str,
    auto_delete: Option<Duration>,
    values: &Values,
) -> Result<(), ModuleError> {
    let content = truncate(&render(template, values), CONTENT_CHARS);
    let sent = ctx.api.send_message(message.channel_id, content).await?;
    if let Some(after) = auto_delete {
        let payload = Cleanup {
            channel: message.channel_id.get(),
            message: sent.id.get(),
        };
        let scheduled = ctx
            .scheduler()
            .job(CLEANUP_JOB, &payload)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        scheduled
            .after(after)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    Ok(())
}

/// Posts the text to the channel the rule names. Without a text, it says who, which rule and what
/// was found, with a link when there is a message.
async fn alert(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    channel: ChannelId,
    template: Option<&str>,
) -> Result<(), ModuleError> {
    let body = alert_text(ctx, rule, hit, context, template);
    ctx.api
        .send_message(channel, truncate(&body, CONTENT_CHARS))
        .await?;
    Ok(())
}

/// The text of an alert: the template given, or the default that says who, which rule and what was
/// found, with a link when there is a message.
fn alert_text(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    template: Option<&str>,
) -> String {
    if let Some(template) = template {
        return render(template, &values(ctx, rule, hit, context, None));
    }
    let args = Args::new()
        .with("rule", rule.name.as_str())
        .with("user", context.user_id().map_or_else(String::new, mention))
        .with("detail", hit.detail.as_str())
        .with(
            "link",
            context
                .message
                .as_ref()
                .map_or_else(String::new, |message| message_link_of(ctx, message)),
        );
    let key = match (&context.message, context.user_id()) {
        (Some(_), _) => "alert-default",
        (None, Some(_)) => "alert-default-member",
        (None, None) => "alert-default-plain",
    };
    text(ctx, key, &args)
}

/// Records the hit as an `automod` case and posts it to the log. A log that cannot take it does not
/// undo the record.
async fn record(
    ctx: &EventContext,
    bot: &Moderator,
    target: UserId,
    reason: &str,
) -> Result<(), ModuleError> {
    let kind = CaseKind::new("automod").map_err(|error| ModuleError::other(error.to_string()))?;
    let case = ctx
        .cases()
        .create(NewCase {
            kind,
            target,
            moderator: bot.id,
            reason: Some(reason.to_owned()),
            expires_at: None,
        })
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let _ = modlog::post(ctx, &case).await;
    Ok(())
}

/// The reason of a punishment or a deletion: the template given, or the rule and what it found.
fn reason_for(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    target: Option<UserId>,
    template: Option<&str>,
) -> String {
    match template {
        Some(template) => render(template, &values(ctx, rule, hit, context, target)),
        None => text(
            ctx,
            "reason-automod",
            &Args::new()
                .with("rule", rule.name.as_str())
                .with("detail", hit.detail.as_str()),
        ),
    }
}

/// The values the templates of the rule read, for the member `target` (the member of the context
/// when none is given).
fn values(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
    target: Option<UserId>,
) -> Values {
    let user = target.or_else(|| context.user_id());
    let username = match (&context.user, user) {
        (Some(who), Some(id)) if who.id == id => who.name.clone(),
        (_, Some(id)) => id.get().to_string(),
        (_, None) => String::new(),
    };
    let count = u32::try_from(hit.count).unwrap_or(u32::MAX);
    let values = Values::new()
        .with("user", user.map_or_else(String::new, mention))
        .with(
            "user_id",
            user.map_or_else(String::new, |id| id.get().to_string()),
        )
        .with("username", username)
        .with(
            "channel",
            context
                .channel
                .map_or_else(String::new, |channel| format!("<#{}>", channel.get())),
        )
        .with("rule", rule.name.as_str())
        .with("detail", hit.detail.as_str())
        .with(
            "message_link",
            context
                .message
                .as_ref()
                .map_or_else(String::new, |message| message_link_of(ctx, message)),
        )
        .with("guild", ctx.guild_id.get().to_string())
        .with("count", f64::from(count));
    match &context.source {
        Source::Counter {
            counter, trigger, ..
        } => values
            .with("counter", counter.as_str())
            .with("trigger", trigger.as_str()),
        Source::Antiraid { level, old } => values
            .with("level", level.as_str())
            .with("old", old.as_str()),
        _ => values,
    }
}

/// Renders a template. The configuration checked it when the rule was read, so a template that
/// does not parse is shown as it was written.
fn render(template: &str, values: &Values) -> String {
    match Template::parse(template) {
        Ok(parsed) => parsed.render(values),
        Err(_) => template.to_owned(),
    }
}

fn message_link_of(ctx: &EventContext, message: &MessageInfo) -> String {
    format!(
        "https://discord.com/channels/{}/{}/{}",
        ctx.guild_id.get(),
        message.channel_id.get(),
        message.id.get()
    )
}

async fn send(ctx: &EventContext, channel: ChannelId, embed: Embed) -> Result<(), ModuleError> {
    ctx.api
        .execute(Discord::SendMessage {
            channel_id: channel,
            content: None,
            embeds: vec![embed],
            reply_to: None,
            mentions: MentionPolicy::None,
        })
        .await
        .map(drop)
        .map_err(ModuleError::from)
}

/// The settings the punishments run with. A role timeout gets its role as moderation does: the
/// configured one, or the one Closure finds or creates (see [`ensure_role`]). Only a rule that
/// times out asks for it.
async fn punish_settings(
    ctx: &EventContext,
    config: &Config,
    rule: &Rule,
) -> Result<Settings, ModuleError> {
    let times_out = rule
        .actions
        .iter()
        .any(|action| matches!(action, Action::Timeout { .. }));
    let timeout_role = if config.timeout_method == Method::Role && times_out {
        let configured = crate::config::role_of(config.timeout_role.as_deref());
        Some(
            ensure_role(ctx, RestrictionRole::Timeout, configured)
                .await
                .map_err(|error| ModuleError::other(error.to_string()))?,
        )
    } else {
        None
    };
    Ok(Settings {
        dm: config.dm_on_action,
        timeout: config.timeout_method,
        timeout_role,
        voice: Method::Discord,
        voice_role: None,
        strip: Strip {
            remove: config.remove_roles_on_mute.selection(),
            restore: config.restore_roles_on_mute.selection(),
        },
    })
}

fn mention(user: UserId) -> String {
    format!("<@{}>", user.get())
}

pub(crate) fn text(ctx: &EventContext, key: &str, args: &Args) -> String {
    ctx.t_in(ctx.locale, key, args)
}

/// The payload of the job that removes a reply.
#[derive(Debug, Serialize, Deserialize)]
struct Cleanup {
    channel: u64,
    message: u64,
}

/// Removes a reply whose time is up. A reply that is gone already is not an error.
pub async fn cleanup(cx: JobCx) -> Result<(), ModuleError> {
    let payload: Cleanup = cx.payload()?;
    let channel: Option<ChannelId> = Id::new_checked(payload.channel);
    let message: Option<MessageId> = Id::new_checked(payload.message);
    let (Some(channel), Some(message)) = (channel, message) else {
        return Err(ModuleError::other("a cleanup names no message"));
    };
    match cx
        .api()
        .delete_message(
            channel,
            message,
            Reason::truncated("automod: a reply whose time is up"),
        )
        .await
    {
        Ok(()) | Err(DiscordError::NotFound) => Ok(()),
        Err(error) => Err(ModuleError::from(error)),
    }
}
