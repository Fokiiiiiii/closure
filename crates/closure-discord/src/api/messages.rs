use closure_core::action::{Emoji, MentionPolicy, MessageCursor, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, GuildId, MessageId, UserId};
use twilight_http::request::AuditLogReason;
use twilight_http::request::channel::reaction::RequestReactionType;
use twilight_model::channel::message::{AllowedMentions, Embed};

use super::{TwilightApi, read, read_list, run};

/// The form Discord takes for an emoji in a reaction path or body.
fn request_emoji(emoji: &Emoji) -> RequestReactionType<'_> {
    match emoji {
        Emoji::Unicode { value } => RequestReactionType::Unicode { name: value },
        Emoji::Custom { id, name } => RequestReactionType::Custom {
            id: *id,
            name: name.as_deref(),
        },
    }
}

/// Nobody is notified unless the caller explicitly lists them; `@everyone`, `@here` and
/// arbitrary role mentions can never be produced by echoing user text.
pub(super) fn allowed_mentions(policy: &MentionPolicy) -> AllowedMentions {
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
    pub(super) async fn send_file(
        &self,
        channel: ChannelId,
        content: Option<&str>,
        embeds: &[Embed],
        file: &closure_core::action::FileUpload,
    ) -> Result<Outcome, DiscordError> {
        // A file is sent without pinging anyone: its name and text come from the host, not a module.
        let allowed = allowed_mentions(&MentionPolicy::None);
        let attachments = [twilight_model::http::attachment::Attachment::from_bytes(
            file.filename.clone(),
            file.data.clone(),
            0,
        )];
        let mut request = self
            .http
            .create_message(channel)
            .allowed_mentions(Some(&allowed))
            .attachments(&attachments);
        if let Some(content) = content {
            request = request.content(content);
        }
        if !embeds.is_empty() {
            request = request.embeds(embeds);
        }
        read(request)
            .await
            .map(|message| Outcome::Message(Box::new(message)))
    }

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

    /// Opens the private channel with `user` and sends the message there. The user must be a
    /// member of `guild`: the cache is trusted when it knows, otherwise Discord is asked, and
    /// when that fails nothing is sent.
    pub(super) async fn send_direct_message(
        &self,
        guild: GuildId,
        user: UserId,
        content: Option<&str>,
        embeds: &[Embed],
    ) -> Result<Outcome, DiscordError> {
        self.ensure_member(guild, user).await?;
        let channel = read(self.http.create_private_channel(user)).await?;
        let allowed = AllowedMentions::default();
        let mut request = self
            .http
            .create_message(channel.id)
            .allowed_mentions(Some(&allowed));
        if let Some(content) = content {
            request = request.content(content);
        }
        if !embeds.is_empty() {
            request = request.embeds(embeds);
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
        run(self
            .http
            .create_reaction(channel, message, &request_emoji(emoji)))
        .await
    }

    pub(super) async fn get_message(
        &self,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<Outcome, DiscordError> {
        read(self.http.message(channel, message))
            .await
            .map(|message| Outcome::Message(Box::new(message)))
    }

    /// The newest messages when there is no cursor.
    pub(super) async fn get_messages(
        &self,
        channel: ChannelId,
        cursor: Option<MessageCursor>,
        limit: u16,
    ) -> Result<Outcome, DiscordError> {
        let request = self.http.channel_messages(channel).limit(limit);
        let messages = match cursor {
            None => read_list(request).await,
            Some(MessageCursor::Before(message)) => read_list(request.before(message)).await,
            Some(MessageCursor::After(message)) => read_list(request.after(message)).await,
            Some(MessageCursor::Around(message)) => read_list(request.around(message)).await,
        }?;
        Ok(Outcome::Messages(messages))
    }

    pub(super) async fn get_pins(&self, channel: ChannelId) -> Result<Outcome, DiscordError> {
        let listing = read(self.http.pins(channel)).await?;
        Ok(Outcome::Messages(
            listing.items.into_iter().map(|pin| pin.message).collect(),
        ))
    }

    pub(super) async fn pin_message(
        &self,
        channel: ChannelId,
        message: MessageId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .create_pin(channel, message)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn unpin_message(
        &self,
        channel: ChannelId,
        message: MessageId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .delete_pin(channel, message)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn crosspost_message(
        &self,
        channel: ChannelId,
        message: MessageId,
    ) -> Result<Outcome, DiscordError> {
        read(self.http.crosspost_message(channel, message))
            .await
            .map(|message| Outcome::Message(Box::new(message)))
    }

    pub(super) async fn remove_reaction(
        &self,
        channel: ChannelId,
        message: MessageId,
        emoji: &Emoji,
        user: UserId,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .delete_reaction(channel, message, &request_emoji(emoji), user))
        .await
    }

    pub(super) async fn remove_all_reactions(
        &self,
        channel: ChannelId,
        message: MessageId,
        emoji: Option<&Emoji>,
    ) -> Result<Outcome, DiscordError> {
        match emoji {
            None => run(self.http.delete_all_reactions(channel, message)).await,
            Some(emoji) => {
                run(self
                    .http
                    .delete_all_reaction(channel, message, &request_emoji(emoji)))
                .await
            }
        }
    }

    pub(super) async fn get_reaction_users(
        &self,
        channel: ChannelId,
        message: MessageId,
        emoji: &Emoji,
        after: Option<UserId>,
        limit: u16,
    ) -> Result<Outcome, DiscordError> {
        let emoji = request_emoji(emoji);
        let mut request = self.http.reactions(channel, message, &emoji).limit(limit);
        if let Some(after) = after {
            request = request.after(after);
        }
        read_list(request).await.map(Outcome::Users)
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
