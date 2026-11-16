//! Channel messages that notify nobody, such as the link preview a bot posts under a user's link.

use closure_core::action::{Action, FileUpload, MentionPolicy, Outcome};
use closure_core::error::DiscordError;
use closure_core::event::ChannelId;
use closure_core::module::EventContext;
use twilight_model::channel::message::{Component, Embed};
use twilight_model::id::Id;
use twilight_model::id::marker::MessageMarker;

/// Sends `content` to `channel` without notifications, so the author's own message does not ping twice.
pub async fn send_silent(
    ctx: &EventContext,
    channel: ChannelId,
    content: impl Into<String>,
) -> Result<Outcome, DiscordError> {
    ctx.api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: Some(content.into()),
            embeds: Vec::new(),
            reply_to: None,
            mentions: MentionPolicy::None,
            silent: true,
        })
        .await
}

/// What a silent post carries. The default is an empty post; text, embeds, buttons, files and
/// the message it answers are all optional.
#[derive(Debug, Default)]
pub struct SilentPost {
    pub content: Option<String>,
    pub embeds: Vec<Embed>,
    pub components: Vec<Component>,
    pub files: Vec<FileUpload>,
    pub reply_to: Option<Id<MessageMarker>>,
}

/// Posts `post` to `channel` without notifications and returns the new message's id.
pub async fn send_silent_with(
    ctx: &EventContext,
    channel: ChannelId,
    post: SilentPost,
) -> Result<Option<Id<MessageMarker>>, DiscordError> {
    let outcome = ctx
        .api
        .execute(Action::PostMessage {
            channel_id: channel,
            message_id: None,
            reply_to: post.reply_to,
            content: post.content,
            embeds: post.embeds,
            components: post.components,
            files: post.files,
            silent: true,
        })
        .await?;
    Ok(match outcome {
        Outcome::Message(message) => Some(message.id),
        _ => None,
    })
}
