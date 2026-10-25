//! Private messages to members of the server: a warning or a ban explained to the user.

use closure_core::action::{Action, Outcome};
use closure_core::error::DiscordError;
use closure_core::event::{MessageId, UserId};
use closure_core::module::EventContext;
use twilight_model::channel::message::Embed;

use crate::UserError;

/// What a direct message says: text, an embed, or both.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DirectMessage {
    pub content: Option<String>,
    pub embeds: Vec<Embed>,
}

impl DirectMessage {
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            embeds: Vec::new(),
        }
    }

    #[must_use]
    pub fn embed(embed: Embed) -> Self {
        Self {
            content: None,
            embeds: vec![embed],
        }
    }
}

/// How a direct message went. A user who closed direct messages from the bot is `Closed`: that
/// is an answer, not a failure, since a moderation step goes on without the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    Delivered(MessageId),
    Closed,
}

/// Sends `message` privately to `user`, who has to be a member of the guild of `ctx`. The module
/// needs the capability `SendDirectMessages`.
pub async fn send(
    ctx: &EventContext,
    user: UserId,
    message: DirectMessage,
) -> Result<Sent, UserError> {
    let action = Action::SendDirectMessage {
        guild_id: ctx.guild_id,
        user_id: user,
        content: message.content,
        embeds: message.embeds,
    };
    match ctx.api.execute(action).await {
        Ok(Outcome::Message(sent)) => Ok(Sent::Delivered(sent.id)),
        Ok(_) => {
            Err(DiscordError::Fatal("unexpected outcome for `send_direct_message`".into()).into())
        }
        Err(DiscordError::DirectMessagesClosed) => Ok(Sent::Closed),
        Err(error) => Err(error.into()),
    }
}
