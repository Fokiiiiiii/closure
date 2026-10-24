use closure_core::action::{Emoji, MentionPolicy, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, MessageId};
use twilight_http::request::AuditLogReason;
use twilight_http::request::channel::reaction::RequestReactionType;
use twilight_model::channel::message::{AllowedMentions, Embed};

use super::{TwilightApi, read, run};

/// Nobody is notified unless the caller explicitly lists them; `@everyone`, `@here` and
/// arbitrary role mentions can never be produced by echoing user text.
fn allowed_mentions(policy: &MentionPolicy) -> AllowedMentions {
    match policy {
        MentionPolicy::None => AllowedMentions::default(),
        MentionPolicy::Users(users) => AllowedMentions {
            users: users.clone(),
            ..AllowedMentions::default()
        },
        MentionPolicy::Roles(roles) => AllowedMentions {
            roles: roles.clone(),
            ..AllowedMentions::default()
        },
    }
}

impl TwilightApi {
    pub(super) async fn send_message(
        &self,
        channel: ChannelId,
        content: Option<&str>,
        embeds: &[Embed],
        reply_to: Option<MessageId>,
        mentions: &MentionPolicy,
    ) -> Result<Outcome, DiscordError> {
        let allowed = allowed_mentions(mentions);
        let mut request = self
            .http
            .create_message(channel)
            .allowed_mentions(Some(&allowed));
        if let Some(content) = content {
            request = request.content(content);
        }
        if !embeds.is_empty() {
            request = request.embeds(embeds);
        }
        if let Some(message) = reply_to {
            // A reply to a message that has since been deleted (say, by auto moderation) should
            // still be sent as a plain message rather than fail.
            request = request.reply(message).fail_if_not_exists(false);
        }
        read(request)
            .await
            .map(|message| Outcome::Message(Box::new(message)))
    }

    /// `None` leaves a field unchanged, unlike Twilight where `None` clears it.
    pub(super) async fn edit_message(
        &self,
        channel: ChannelId,
        message: MessageId,
        content: Option<&str>,
        embeds: Option<&[Embed]>,
    ) -> Result<Outcome, DiscordError> {
        let allowed = AllowedMentions::default();
        let mut request = self
            .http
            .update_message(channel, message)
            .allowed_mentions(Some(&allowed));
        if let Some(content) = content {
            request = request.content(Some(content));
        }
        if let Some(embeds) = embeds {
            request = request.embeds(Some(embeds));
        }
        read(request)
            .await
            .map(|message| Outcome::Message(Box::new(message)))
    }

    pub(super) async fn delete_message(
        &self,
        channel: ChannelId,
        message: MessageId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .delete_message(channel, message)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn bulk_delete(
        &self,
        channel: ChannelId,
        messages: &[MessageId],
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .delete_messages(channel, messages)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn add_reaction(
        &self,
        channel: ChannelId,
        message: MessageId,
        emoji: &Emoji,
    ) -> Result<Outcome, DiscordError> {
        let emoji = match emoji {
            Emoji::Unicode { value } => RequestReactionType::Unicode { name: value },
            Emoji::Custom { id, name } => RequestReactionType::Custom {
                id: *id,
                name: name.as_deref(),
            },
        };
        run(self.http.create_reaction(channel, message, &emoji)).await
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    #[test]
    fn default_mentions_notify_nobody() {
        let allowed = allowed_mentions(&MentionPolicy::None);
        assert!(allowed.parse.is_empty());
        assert!(allowed.users.is_empty());
        assert!(allowed.roles.is_empty());
        assert!(!allowed.replied_user);
    }

    #[test]
    fn explicit_lists_are_the_only_exceptions() {
        let users = allowed_mentions(&MentionPolicy::Users(vec![Id::new(1)]));
        assert_eq!(users.users, [Id::new(1)]);
        assert!(users.roles.is_empty() && users.parse.is_empty());

        let roles = allowed_mentions(&MentionPolicy::Roles(vec![Id::new(2)]));
        assert_eq!(roles.roles, [Id::new(2)]);
        assert!(roles.users.is_empty() && roles.parse.is_empty());
    }
}
