use closure_core::action::{Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, GuildId, UserId};
use twilight_http::request::AuditLogReason;

use super::{TwilightApi, run};

impl TwilightApi {
    pub(super) async fn set_voice_deafen(
        &self,
        guild: GuildId,
        user: UserId,
        deaf: bool,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .update_guild_member(guild, user)
            .deaf(deaf)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn move_member(
        &self,
        guild: GuildId,
        user: UserId,
        channel: ChannelId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .update_guild_member(guild, user)
            .channel_id(Some(channel))
            .reason(reason.as_str()))
        .await
    }

    /// Discord has no separate call for this: a member with no voice channel is removed from voice.
    pub(super) async fn disconnect_member(
        &self,
        guild: GuildId,
        user: UserId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .update_guild_member(guild, user)
            .channel_id(None)
            .reason(reason.as_str()))
        .await
    }
}
