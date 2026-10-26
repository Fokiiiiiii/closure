use closure_core::action::{MentionPolicy, Outcome, Reason, WebhookToken};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, GuildId};
use twilight_http::request::AuditLogReason;
use twilight_model::channel::message::Embed;
use twilight_model::id::Id;
use twilight_model::id::marker::WebhookMarker;

use super::messages::allowed_mentions;
use super::{TwilightApi, read, read_list, run};

/// Refuses a webhook whose guild is not `guild`, or whose guild Discord does not name.
fn same_guild(found: Option<GuildId>, guild: GuildId) -> Result<(), DiscordError> {
    if found == Some(guild) {
        Ok(())
    } else {
        Err(DiscordError::Forbidden(
            "the webhook is not of the guild being served".into(),
        ))
    }
}

impl TwilightApi {
    pub(super) async fn create_webhook(
        &self,
        channel: ChannelId,
        name: &str,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        read(
            self.http
                .create_webhook(channel, name)
                .reason(reason.as_str()),
        )
        .await
        .map(|hook| Outcome::Webhook(Box::new(hook)))
    }

    pub(super) async fn list_webhooks(&self, channel: ChannelId) -> Result<Outcome, DiscordError> {
        read_list(self.http.channel_webhooks(channel))
            .await
            .map(Outcome::Webhooks)
    }

    /// Discord deletes a webhook by its id alone, so the webhook's guild is read first.
    pub(super) async fn delete_webhook(
        &self,
        guild: GuildId,
        webhook: Id<WebhookMarker>,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let hook = read(self.http.webhook(webhook)).await?;
        same_guild(hook.guild_id, guild)?;
        run(self.http.delete_webhook(webhook).reason(reason.as_str())).await
    }

    /// The webhook's guild is read with its token first, as posting needs the token anyway. The
    /// reply is the message that was posted.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_webhook(
        &self,
        guild: GuildId,
        webhook: Id<WebhookMarker>,
        token: &WebhookToken,
        content: Option<&str>,
        embeds: &[Embed],
        username: Option<&str>,
        avatar_url: Option<&str>,
        mentions: &MentionPolicy,
    ) -> Result<Outcome, DiscordError> {
        let hook = read(self.http.webhook(webhook).token(token.expose())).await?;
        same_guild(hook.guild_id, guild)?;
        let allowed = allowed_mentions(mentions);
        let mut request = self
            .http
            .execute_webhook(webhook, token.expose())
            .allowed_mentions(Some(&allowed));
        if let Some(content) = content {
            request = request.content(content);
        }
        if !embeds.is_empty() {
            request = request.embeds(embeds);
        }
        if let Some(username) = username {
            request = request.username(username);
        }
        if let Some(avatar_url) = avatar_url {
            request = request.avatar_url(avatar_url);
        }
        read(request.wait())
            .await
            .map(|message| Outcome::Message(Box::new(message)))
    }
}
