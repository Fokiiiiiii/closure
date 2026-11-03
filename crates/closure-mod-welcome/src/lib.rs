//! Welcome members when they join a server.

use std::time::Duration;

use closure_core::action::{Action, MentionPolicy, Outcome, Reason};
use closure_core::capability::Capability;
use closure_core::error::ModuleError;
use closure_core::event::{Event, EventKind};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::recent::{Burst, Claim, Recent, RecentActions, RecentFilter, RecentLimits};
use closure_sdk::template::{Template, Values};
use closure_sdk::{CoreError, Intents, JobCx, Locale, ModuleBuilder, SdkModule, Subscription};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
const WINDOW: Duration = Duration::from_secs(60);
const MAX_JOINS: u64 = 5;
const FLOOD_KEY: &str = "welcome-join-flood";
const DELETE_JOB: &str = "delete_welcome";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub channel: Option<String>,
    pub message: Option<String>,
    pub delete_after: Option<String>,
    pub include_bots: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeleteWelcome {
    channel: u64,
    message: u64,
}

fn parse_config(
    ctx: &EventContext,
) -> Result<
    (
        Config,
        Option<closure_core::event::ChannelId>,
        Option<Duration>,
    ),
    ModuleError,
> {
    let config: Config = ctx.parse_config()?;
    if let Some(message) = config.message.as_deref() {
        validate_template(message).map_err(ModuleError::Config)?;
    }
    let channel = channel_id(&config)?;
    let delete_after = config
        .delete_after
        .as_deref()
        .map(validate_delete_after)
        .transpose()
        .map_err(ModuleError::Config)?;
    Ok((config, channel, delete_after))
}

fn validate_template(message: &str) -> Result<Template, String> {
    if message.chars().count() > 2_000 {
        return Err("welcome message exceeds 2000 characters".into());
    }
    let template = Template::parse(message).map_err(|error| error.to_string())?;
    if !template.uses_only(&["user", "username", "server", "count"]) {
        return Err("welcome message has an unknown variable".into());
    }
    Ok(template)
}

fn validate_delete_after(text: &str) -> Result<Duration, String> {
    let span = closure_sdk::Span::parse(text).map_err(|error| error.to_string())?;
    if span.is_moment() {
        return Err("delete_after must be a duration".into());
    }
    let duration = span.resolve_at(0, 0).map_err(|error| error.to_string())?;
    if !(Duration::from_secs(5)..=Duration::from_hours(168)).contains(&duration) {
        return Err("delete_after must be between 5 seconds and 7 days".into());
    }
    Ok(duration)
}

fn channel_id(config: &Config) -> Result<Option<closure_core::event::ChannelId>, ModuleError> {
    config
        .channel
        .as_deref()
        .map(|channel| {
            channel
                .parse::<u64>()
                .ok()
                .filter(|channel| *channel > 0)
                .map(closure_core::event::ChannelId::new)
                .ok_or_else(|| ModuleError::Config("channel must be a snowflake string".into()))
        })
        .transpose()
}

async fn member_joined(
    ctx: EventContext,
    event: Event,
    recent: RecentActions,
) -> Result<Flow, ModuleError> {
    let Event::MemberJoined(member) = event else {
        return Ok(Flow::Continue);
    };
    let (config, channel, delete_after) = parse_config(&ctx)?;
    let Some(channel) = channel else {
        return Ok(Flow::Continue);
    };
    if member.user.bot && !config.include_bots {
        return Ok(Flow::Continue);
    }

    let now = ctx.now();
    recent.record(
        ctx.guild_id,
        Recent {
            kind: "welcome_join".into(),
            subject: member.user.id.get(),
            group: None,
            count: 1,
            at: now,
            messages: Vec::new(),
            tag: None,
        },
    );
    let burst = Burst {
        kind: "welcome_join",
        subject: None,
        within: WINDOW,
        filter: &RecentFilter::default(),
        handled_key: FLOOD_KEY,
        handled_until: now.saturating_add(i64::try_from(WINDOW.as_secs()).unwrap_or(i64::MAX)),
    };
    if matches!(
        recent.claim_burst(ctx.guild_id, now, &burst, |entries| {
            let count = entries
                .iter()
                .map(|entry| u64::from(entry.count))
                .sum::<u64>();
            (count > MAX_JOINS).then_some(count)
        }),
        Claim::Handled | Claim::Counted { .. }
    ) {
        tracing::debug!(guild = %ctx.guild_id, "welcome join flood limit reached");
        return Ok(Flow::Continue);
    }

    send_welcome(&ctx, &member, &config, channel, delete_after).await?;
    Ok(Flow::Continue)
}

async fn send_welcome(
    ctx: &EventContext,
    member: &closure_core::event::MemberInfo,
    config: &Config,
    channel: closure_core::event::ChannelId,
    delete_after: Option<Duration>,
) -> Result<(), ModuleError> {
    let template = match config.message.as_deref() {
        Some(message) => validate_template(message).map_err(ModuleError::Config)?,
        None => {
            Template::parse(&ctx.t_in(ctx.locale, "welcome-default", &closure_sdk::Args::new()))
                .map_err(|error| ModuleError::Config(error.to_string()))?
        }
    };
    let mut values = json!({
        "user": format!("<@{}>", member.user.id),
        "username": member.user.name,
    });
    if template.uses("server") || template.uses("count") {
        match ctx
            .api
            .execute(Action::GetGuild {
                guild_id: ctx.guild_id,
            })
            .await
        {
            Ok(Outcome::Guild(guild)) => {
                if template.uses("server") {
                    values["server"] = Value::String(guild.name);
                }
                if template.uses("count")
                    && let Some(count) = guild.member_count
                {
                    values["count"] = Value::String(count.to_string());
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(?error, guild = %ctx.guild_id, "could not read guild details for welcome");
            }
        }
    }
    let content = template.render(&Values::from_json(values).unwrap_or_default());
    let message = match ctx
        .api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: Some(content),
            embeds: Vec::new(),
            reply_to: None,
            mentions: MentionPolicy::Users(vec![member.user.id]),
        })
        .await
    {
        Ok(Outcome::Message(message)) => message,
        Ok(_) => return Ok(()),
        Err(error) => {
            tracing::debug!(?error, guild = %ctx.guild_id, "could not send welcome message");
            return Ok(());
        }
    };
    if let Some(delay) = delete_after {
        let job = DeleteWelcome {
            channel: channel.get(),
            message: message.id.get(),
        };
        match ctx.scheduler().job(DELETE_JOB, &job) {
            Ok(job) => {
                if let Err(error) = job.after(delay).await {
                    tracing::debug!(?error, "could not schedule welcome deletion");
                }
            }
            Err(error) => tracing::debug!(?error, "could not schedule welcome deletion"),
        }
    }
    Ok(())
}

async fn delete_welcome(cx: JobCx) -> Result<(), ModuleError> {
    let job: DeleteWelcome = cx.payload()?;
    let result = cx
        .ctx()
        .api
        .execute(Action::DeleteMessage {
            channel_id: closure_core::event::ChannelId::new(job.channel),
            message_id: closure_core::event::MessageId::new(job.message),
            reason: Reason::new(cx.t("welcome-delete-reason", &closure_sdk::Args::new()))?,
        })
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) => {
            tracing::debug!(?error, "could not delete expired welcome message");
            Ok(())
        }
    }
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let recent = RecentActions::new(RecentLimits::default());
    Ok(ModuleBuilder::new("welcome", "Welcome", env!("CARGO_PKG_VERSION"))?
        .capability(Capability::SendMessages)
        .capability(Capability::ManageMessages)
        .capability(Capability::ReadGuild)
        .intents(Intents::GUILD_MEMBERS)
        .config(
            json!({
                "type": "object",
                "properties": {
                    "channel": {
                        "type": ["string", "null"],
                        "pattern": "^[0-9]{1,20}$",
                        "description": "welcome-channel",
                        "x-ui": { "widget": "channel", "level": "basic" }
                    },
                    "message": {
                        "type": ["string", "null"],
                        "maxLength": 2000,
                        "pattern": "^([^{}]|\\{\\{|\\}\\}|\\{(user|username|server|count)\\})*$",
                        "description": "welcome-message"
                    },
                    "delete_after": {
                        "type": ["string", "null"],
                        "pattern": "^(?:(?:[5-9]|[1-9][0-9]{1,4}|[1-5][0-9]{5}|60[0-3][0-9]{3}|604[0-7][0-9]{2}|604800)s|(?:[1-9]|[1-9][0-9]{1,3}|100[0-7][0-9]|10080)m|(?:[1-9]|[1-9][0-9]|1[0-5][0-9]|16[0-8])h|[1-7]d)$",
                        "description": "welcome-delete-after"
                    },
                    "include_bots": {
                        "type": "boolean",
                        "description": "welcome-include-bots",
                        "x-ui": { "level": "basic" }
                    }
                },
                "additionalProperties": false
            }),
            json!({
                "channel": null,
                "message": null,
                "delete_after": null,
                "include_bots": false
            }),
        )
        .job(DELETE_JOB, delete_welcome)
        .on(
            Subscription::new(EventKind::MemberJoined),
            move |ctx, event| {
                let recent = recent.clone();
                member_joined(ctx, event, recent)
            },
        )
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA))
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
