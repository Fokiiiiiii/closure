//! What a rule does once it hits: its actions in order, the record they leave, and the report of a
//! dry run.

use std::collections::BTreeMap;
use std::time::Duration;

use closure_core::action::{Action as Discord, MentionPolicy, Reason};
use closure_core::error::DiscordError;
use closure_core::event::MessageId;
use closure_sdk::embed::{Color, EmbedBuilder, truncate};
use closure_sdk::modlog;
use closure_sdk::punish::{Done, Method, Moderator, Punisher, Settings};
use closure_sdk::template::{Template, Values};
use closure_sdk::{
    Args, CaseKind, ChannelId, EventContext, JobCx, MessageInfo, ModuleError, NewCase, UserError,
    UserId,
};
use serde::{Deserialize, Serialize};
use twilight_model::channel::message::Embed;
use twilight_model::id::Id;

use crate::config::{Action, Config, Rule, action_key};
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

/// What an action needs: the hit, the message it is about, and the values its templates read.
struct Scene<'a> {
    ctx: &'a EventContext,
    rule: &'a Rule,
    hit: &'a Hit,
    message: &'a MessageInfo,
    values: &'a Values,
}

/// Whether an action punished the member, or only touched the messages.
enum Ran {
    Other,
    Punished,
}

/// Runs the rule's actions in order. A failing one does not stop the next; the first error is
/// returned at the end. When nothing punished the member, the hit is recorded as an `automod`
/// case, so that a deletion still shows in the log. A later message of a burst only has its
/// messages deleted.
pub async fn apply(
    ctx: &EventContext,
    rule: &Rule,
    hit: &Hit,
    message: &MessageInfo,
    config: &Config,
) -> Result<(), ModuleError> {
    let values = values(ctx, rule, hit, message);
    let scene = Scene {
        ctx,
        rule,
        hit,
        message,
        values: &values,
    };
    let bot = Moderator::bot(ctx, BOT_NAME)?;
    let punisher = Punisher::new(ctx, bot.clone(), settings(config));
    let mut first_error = None;
    let mut ran = false;
    let mut did_punish = false;
    for action in &rule.actions {
        if hit.join && !matches!(action, Action::Delete(_)) {
            continue;
        }
        match run(&scene, &punisher, config, action).await {
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
        && !hit.join
        && let Err(error) = record(ctx, &bot, message.author.id, &reason_of(&scene, None)).await
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

async fn run(
    scene: &Scene<'_>,
    punisher: &Punisher<'_>,
    config: &Config,
    action: &Action,
) -> Result<Ran, ModuleError> {
    let target = scene.message.author.id;
    match action {
        Action::Delete(_) => delete(scene).await.map(|()| Ran::Other),
        Action::Purge { within } => purge(scene, *within).await.map(|()| Ran::Other),
        Action::Warn { reason } => {
            let expires_at = scene.ctx.now() + i64::from(config.warn_expire_days) * 86_400;
            let reason = Some(reason_of(scene, reason.as_deref()));
            punished(punisher.warn(target, reason, Some(expires_at)).await)
        }
        Action::Timeout { duration, reason } => {
            let reason = Some(reason_of(scene, reason.as_deref()));
            punished(punisher.timeout(target, *duration, reason).await)
        }
        Action::Kick { reason } => {
            let reason = Some(reason_of(scene, reason.as_deref()));
            punished(punisher.kick(target, reason).await)
        }
        Action::Ban {
            duration,
            delete_days,
            reason,
        } => {
            let seconds = delete_days.saturating_mul(DAY_SECONDS);
            let reason = Some(reason_of(scene, reason.as_deref()));
            punished(punisher.ban(target, *duration, seconds, reason).await)
        }
        Action::Reply { text, auto_delete } => {
            reply(scene, text, *auto_delete).await.map(|()| Ran::Other)
        }
        Action::Alert { channel, text } => alert(scene, *channel, text.as_deref())
            .await
            .map(|()| Ran::Other),
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

/// Deletes the messages of the hit.
async fn delete(scene: &Scene<'_>) -> Result<(), ModuleError> {
    let reason = reason_of(scene, None);
    delete_all(scene.ctx, &scene.hit.messages, &reason).await
}

/// Deletes what the member sent within `within`, from the history, and the messages of the hit.
async fn purge(scene: &Scene<'_>, within: Duration) -> Result<(), ModuleError> {
    let since = scene.ctx.now() - i64::try_from(within.as_secs()).unwrap_or(i64::MAX);
    let saved = scene
        .ctx
        .history()
        .by_author(scene.message.author.id, since, BULK_DELETE)
        .await
        .map_err(|error| ModuleError::other(format!("purge: {error}")))?;
    let mut targets = scene.hit.messages.clone();
    targets.extend(
        saved
            .iter()
            .filter(|message| message.deleted_at.is_none())
            .map(|message| (message.channel_id, message.id)),
    );
    let reason = reason_of(scene, None);
    delete_all(scene.ctx, &targets, &reason).await
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

/// Sends the reply in the channel of the hit, optionally removing it again later.
async fn reply(
    scene: &Scene<'_>,
    template: &str,
    auto_delete: Option<Duration>,
) -> Result<(), ModuleError> {
    let content = truncate(&render(template, scene.values), CONTENT_CHARS);
    let sent = scene
        .ctx
        .api
        .send_message(scene.message.channel_id, content)
        .await?;
    if let Some(after) = auto_delete {
        let payload = Cleanup {
            channel: scene.message.channel_id.get(),
            message: sent.id.get(),
        };
        let scheduled = scene
            .ctx
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
/// was found.
async fn alert(
    scene: &Scene<'_>,
    channel: ChannelId,
    template: Option<&str>,
) -> Result<(), ModuleError> {
    let content = match template {
        Some(template) => render(template, scene.values),
        None => text(
            scene.ctx,
            "alert-default",
            &Args::new()
                .with("rule", scene.rule.name.as_str())
                .with("user", mention(scene.message.author.id))
                .with("detail", scene.hit.detail.as_str())
                .with("link", message_link(scene)),
        ),
    };
    scene
        .ctx
        .api
        .send_message(channel, truncate(&content, CONTENT_CHARS))
        .await?;
    Ok(())
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
fn reason_of(scene: &Scene<'_>, template: Option<&str>) -> String {
    match template {
        Some(template) => render(template, scene.values),
        None => text(
            scene.ctx,
            "reason-automod",
            &Args::new()
                .with("rule", scene.rule.name.as_str())
                .with("detail", scene.hit.detail.as_str()),
        ),
    }
}

/// The values the templates of the rule read.
fn values(ctx: &EventContext, rule: &Rule, hit: &Hit, message: &MessageInfo) -> Values {
    let count = u32::try_from(hit.count).unwrap_or(u32::MAX);
    Values::new()
        .with("user", mention(message.author.id))
        .with("user_id", message.author.id.get().to_string())
        .with("username", message.author.name.as_str())
        .with("channel", format!("<#{}>", message.channel_id.get()))
        .with("rule", rule.name.as_str())
        .with("detail", hit.detail.as_str())
        .with("message_link", message_link_of(ctx, message))
        .with("guild", ctx.guild_id.get().to_string())
        .with("count", f64::from(count))
}

/// Renders a template. The configuration checked it when the rule was read, so a template that
/// does not parse is shown as it was written.
fn render(template: &str, values: &Values) -> String {
    match Template::parse(template) {
        Ok(parsed) => parsed.render(values),
        Err(_) => template.to_owned(),
    }
}

fn message_link(scene: &Scene<'_>) -> String {
    message_link_of(scene.ctx, scene.message)
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

fn settings(config: &Config) -> Settings {
    Settings {
        dm: config.dm_on_action,
        timeout: config.timeout_method,
        timeout_role: crate::config::role_of(config.timeout_role.as_deref()),
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
