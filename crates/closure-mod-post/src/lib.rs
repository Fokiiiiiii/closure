//! Sends the posts that were scheduled in the dashboard.
//!
//! The dashboard keeps a post as a job of this module, whole: the channel, the message as it was
//! written (text, embeds, buttons) and the message to replace, if any. When the time comes the job
//! turns it into a Discord message and posts it. A post that can never be sent (the channel is
//! gone, the bot may no longer write there, the message breaks a limit) is dropped with a log
//! line; one that failed for another reason is tried again.

use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, MessageId};
use closure_core::message::MessageSpec;
use closure_sdk::{CoreError, JobCx, Locale, ModuleBuilder, ModuleError, SdkModule};
use serde::Deserialize;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
const SEND: &str = "send";

#[derive(Deserialize)]
struct Scheduled {
    channel: String,
    #[serde(default)]
    edit: Option<String>,
    message: serde_json::Value,
}

async fn send(cx: JobCx) -> Result<(), ModuleError> {
    let job: Scheduled = cx.payload()?;
    let Some(channel) = job
        .channel
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .map(ChannelId::new)
    else {
        tracing::warn!(guild = %cx.guild_id(), "a scheduled post has no channel; it is dropped");
        return Ok(());
    };
    let edit = job
        .edit
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .map(MessageId::new);
    let parts = serde_json::from_value::<MessageSpec>(job.message)
        .map_err(|error| error.to_string())
        .and_then(|spec| {
            spec.into_post(cx.now(), &str::to_owned)
                .map_err(|error| error.to_string())
        });
    let (content, embeds, components) = match parts {
        Ok(parts) => parts,
        Err(error) => {
            tracing::warn!(guild = %cx.guild_id(), %error, "a scheduled post is invalid; it is dropped");
            return Ok(());
        }
    };
    let result = cx
        .api()
        .execute(Action::PostMessage {
            channel_id: channel,
            message_id: edit,
            reply_to: None,
            files: Vec::new(),
            content,
            embeds,
            components,
        })
        .await;
    match result {
        Ok(_) => Ok(()),
        Err(error) if error.is_retryable() => Err(error.into()),
        Err(
            error
            @ (DiscordError::Forbidden(_) | DiscordError::NotFound | DiscordError::Invalid(_)),
        ) => {
            tracing::warn!(guild = %cx.guild_id(), %error, "a scheduled post cannot be sent; it is dropped");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("post", "Scheduled posts", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::Schedule)
            .job(SEND, send)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
