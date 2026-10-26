//! What a rule does once it hits: its actions in order, the record they leave, and the report of a
//! dry run.

use closure_core::action::{Action as Discord, MentionPolicy, Reason};
use closure_core::error::DiscordError;
use closure_sdk::embed::{Color, EmbedBuilder};
use closure_sdk::modlog;
use closure_sdk::punish::{Done, Method, Moderator, Punisher, Settings};
use closure_sdk::{
    Args, CaseKind, ChannelId, EventContext, MessageInfo, ModuleError, NewCase, UserError, UserId,
};
use twilight_model::channel::message::Embed;

use crate::checks::Hit;
use crate::config::{Action, Config, Rule, role_of};

const DAY: i64 = 86_400;
const DAY_SECONDS: u32 = 86_400;
/// The name the bot's own punishments and records carry.
const BOT_NAME: &str = "automod";

/// What a hit is about, with the reason the punishments and the records carry.
struct Hitting<'a> {
    rule: &'a Rule,
    hit: &'a Hit,
    message: &'a MessageInfo,
    reason: String,
}

/// Whether an action punished the member, or only touched the message.
enum Ran {
    Other,
    Punished,
}

/// Runs the rule's actions in order. A failing one does not stop the next; the first error is
/// returned at the end. When nothing punished the member, the hit is recorded as an `automod`
/// case, so that a deletion still shows in the log.
pub async fn apply(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    message: &MessageInfo,
    config: &Config,
) -> Result<(), ModuleError> {
    let hitting = Hitting {
        rule,
        hit,
        message,
        reason: text(
            ctx,
            "reason-automod",
            &Args::new()
                .with("rule", rule.name.as_str())
                .with("detail", hit.detail.as_str()),
        ),
    };
    let bot = Moderator::bot(ctx, BOT_NAME)?;
    let punisher = Punisher::new(ctx, bot.clone(), settings(config));
    let mut first_error = None;
    let mut ran = false;
    let mut did_punish = false;
    for action in &rule.actions {
        match run(ctx, &punisher, config, &hitting, action).await {
            Ok(Ran::Punished) => {
                ran = true;
                did_punish = true;
            }
            Ok(Ran::Other) => ran = true,
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if ran
        && !did_punish
        && let Err(error) = record(ctx, &bot, message.author.id, &hitting.reason).await
    {
        first_error.get_or_insert(error);
    }
    first_error.map_or(Ok(()), Err)
}

/// A dry run: posts what the rule would have done to the log channel, and changes nothing. Without
/// a log channel there is nowhere to say it, so nothing happens.
pub async fn report(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    message: &MessageInfo,
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
    let embed = EmbedBuilder::new()
        .title(text(ctx, "dry-run-title", &Args::new()))
        .color(Color::Info)
        .field(
            text(ctx, "field-rule", &Args::new()),
            rule.name.as_str(),
            true,
        )
        .field(
            text(ctx, "field-member", &Args::new()),
            mention(message.author.id),
            true,
        )
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

/// The key of an action's name, as the status and a dry run show it.
pub fn action_key(action: &Action) -> &'static str {
    match action {
        Action::Delete => "action-delete",
        Action::Warn => "action-warn",
        Action::Timeout(_) => "action-timeout",
        Action::Kick => "action-kick",
        Action::Ban { .. } => "action-ban",
        Action::Notify(_) => "action-notify",
    }
}

async fn run(
    ctx: &EventContext,
    punisher: &Punisher<'_>,
    config: &Config,
    hitting: &Hitting<'_>,
    action: &Action,
) -> Result<Ran, ModuleError> {
    let target = hitting.message.author.id;
    let reason = Some(hitting.reason.clone());
    match action {
        Action::Delete => delete(ctx, hitting.message, &hitting.reason)
            .await
            .map(|()| Ran::Other),
        Action::Notify(channel) => notify(ctx, hitting, *channel).await.map(|()| Ran::Other),
        Action::Warn => {
            let expires_at = ctx.now() + i64::from(config.warn_expire_days) * DAY;
            punished(punisher.warn(target, reason, Some(expires_at)).await)
        }
        Action::Timeout(duration) => punished(punisher.timeout(target, *duration, reason).await),
        Action::Kick => punished(punisher.kick(target, reason).await),
        Action::Ban {
            duration,
            delete_days,
        } => {
            let seconds = delete_days.saturating_mul(DAY_SECONDS);
            punished(punisher.ban(target, *duration, seconds, reason).await)
        }
    }
}

fn punished(result: Result<Done, UserError>) -> Result<Ran, ModuleError> {
    result.map(|_| Ran::Punished).map_err(module_error)
}

fn module_error(error: UserError) -> ModuleError {
    match error {
        UserError::Module(error) => error,
        UserError::Message { key, .. } | UserError::Sdk { key, .. } => ModuleError::other(key),
        other => ModuleError::other(format!("{other:?}")),
    }
}

/// Deletes the message. A message that is gone already is not an error.
async fn delete(
    ctx: &EventContext,
    message: &MessageInfo,
    reason: &str,
) -> Result<(), ModuleError> {
    let deleting = Discord::DeleteMessage {
        channel_id: message.channel_id,
        message_id: message.id,
        reason: Reason::truncated(reason),
    };
    match ctx.api.execute(deleting).await {
        Ok(_) | Err(DiscordError::NotFound) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Posts one embed with the rule, the member, what was found and a link to the message.
async fn notify(
    ctx: &EventContext,
    hitting: &Hitting<'_>,
    channel: ChannelId,
) -> Result<(), ModuleError> {
    let link = format!(
        "https://discord.com/channels/{}/{}/{}",
        ctx.guild_id.get(),
        hitting.message.channel_id.get(),
        hitting.message.id.get()
    );
    let embed = EmbedBuilder::new()
        .title(text(
            ctx,
            "notify-title",
            &Args::new().with("rule", hitting.rule.name.as_str()),
        ))
        .color(Color::Warning)
        .field(
            text(ctx, "field-member", &Args::new()),
            mention(hitting.message.author.id),
            true,
        )
        .field(
            text(ctx, "field-check", &Args::new()),
            hitting.hit.detail.as_str(),
            false,
        )
        .field(text(ctx, "field-message", &Args::new()), link, false)
        .build()
        .map_err(|error| ModuleError::other(error.to_string()))?;
    send(ctx, channel, embed).await
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

fn settings(config: &Config) -> Settings {
    Settings {
        dm: config.dm_on_action,
        timeout: config.timeout_method,
        timeout_role: role_of(config.timeout_role.as_deref()),
        voice: Method::Discord,
        voice_role: None,
    }
}

fn mention(user: UserId) -> String {
    format!("<@{}>", user.get())
}

fn text(ctx: &EventContext, key: &str, args: &Args) -> String {
    ctx.t_in(ctx.locale, key, args)
}
