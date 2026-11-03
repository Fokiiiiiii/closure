use closure_core::action::{FileUpload, MentionPolicy, Outcome, Reason, WebhookToken};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, GuildId};
use std::sync::PoisonError;
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
    pub(super) fn webhook_lock(&self, name: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.webhook_locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    pub(super) async fn resolve_webhook(
        &self,
        guild: GuildId,
        name: &str,
        refresh: bool,
    ) -> Result<ChannelId, DiscordError> {
        let (id, token) = self
            .webhooks
            .get(name)
            .ok_or(DiscordError::WebhookNotRegistered)?;
        let gone_until = self
            .webhook_gone
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .copied();
        if gone_until.is_some_and(|until| (self.clock)() < until) {
            return Err(DiscordError::WebhookGone);
        }
        let known = self
            .webhook_guilds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .copied()
            .filter(|_| gone_until.is_none());
        if let Some((found, channel, checked_at)) = known {
            same_guild(Some(found), guild)?;
            if !refresh || (self.clock)() < checked_at.saturating_add(300) {
                return Ok(channel);
            }
        }
        let response = self
            .http
            .webhook(*id)
            .token(token.expose())
            .await
            .map_err(|e| self.webhook_error(name, *id, &e))?;
        let hook = response
            .model()
            .await
            .map_err(|_| DiscordError::Fatal("could not read webhook response".into()))?;
        same_guild(hook.guild_id, guild)?;
        let channel = hook.channel_id;
        self.webhook_gone
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
        self.webhook_guilds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(*id, (guild, channel, (self.clock)()));
        Ok(channel)
    }

    pub(super) async fn send_via_webhook(
        &self,
        guild: GuildId,
        name: &str,
        content: Option<&str>,
        embeds: &[Embed],
        file: Option<&FileUpload>,
    ) -> Result<Outcome, DiscordError> {
        let (id, token) = self
            .webhooks
            .get(name)
            .ok_or(DiscordError::WebhookNotRegistered)?;
        let lock = self.webhook_lock(name);
        let _guard = lock.lock().await;
        self.resolve_webhook(guild, name, false).await?;
        let allowed = allowed_mentions(&MentionPolicy::None);
        let attachments = file.map(|file| {
            [twilight_model::http::attachment::Attachment::from_bytes(
                file.filename.clone(),
                file.data.clone(),
                0,
            )]
        });
        let mut request = self
            .http
            .execute_webhook(*id, token.expose())
            .allowed_mentions(Some(&allowed));
        if let Some(content) = content {
            request = request.content(content);
        }
        if !embeds.is_empty() {
            request = request.embeds(embeds);
        }
        if let Some(attachments) = &attachments {
            request = request.attachments(attachments);
        }
        let response = request
            .wait()
            .await
            .map_err(|e| self.webhook_error(name, *id, &e))?;
        response
            .model()
            .await
            .map(|message| Outcome::Message(Box::new(message)))
            .map_err(|_| DiscordError::Fatal("could not read webhook message".into()))
    }

    fn webhook_error(
        &self,
        name: &str,
        id: Id<WebhookMarker>,
        error: &twilight_http::Error,
    ) -> DiscordError {
        use twilight_http::error::ErrorType;
        let gone = matches!(error.kind(), ErrorType::Unauthorized)
            || matches!(error.kind(), ErrorType::Response { status, .. } if matches!(status.get(), 401 | 404));
        if gone {
            self.webhook_gone
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(name.to_owned(), (self.clock)().saturating_add(600));
            self.webhook_guilds
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            return DiscordError::WebhookGone;
        }
        match super::map_http_error(error) {
            DiscordError::RateLimited { retry_after } => DiscordError::RateLimited { retry_after },
            DiscordError::Transient(_) => DiscordError::Transient("webhook request failed".into()),
            DiscordError::Forbidden(_) => DiscordError::Forbidden("webhook request failed".into()),
            DiscordError::Invalid(_) => DiscordError::Invalid("webhook request failed".into()),
            _ => DiscordError::Fatal("webhook request failed".into()),
        }
    }
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
