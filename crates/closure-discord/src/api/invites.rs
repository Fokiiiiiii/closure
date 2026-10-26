use closure_core::action::{Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, GuildId};
use twilight_http::request::AuditLogReason;

use super::{TwilightApi, read, read_list, run};

impl TwilightApi {
    pub(super) async fn create_invite(
        &self,
        channel: ChannelId,
        max_age_seconds: u32,
        max_uses: u16,
        temporary: bool,
        unique: bool,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let request = self
            .http
            .create_invite(channel)
            .max_age(max_age_seconds)
            .max_uses(max_uses)
            .temporary(temporary)
            .unique(unique)
            .reason(reason.as_str());
        read(request)
            .await
            .map(|invite| Outcome::Invite(Box::new(invite)))
    }

    pub(super) async fn list_invites(&self, guild: GuildId) -> Result<Outcome, DiscordError> {
        read_list(self.http.guild_invites(guild))
            .await
            .map(Outcome::Invites)
    }

    /// Discord deletes an invite by its code alone, so the guild of the invite is read first and
    /// must be `guild`; otherwise nothing is deleted.
    pub(super) async fn delete_invite(
        &self,
        guild: GuildId,
        code: &str,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let invite = read(self.http.invite(code)).await?;
        if invite.guild.as_ref().map(|found| found.id) != Some(guild) {
            return Err(DiscordError::Forbidden(
                "the invite is not of the guild being served".into(),
            ));
        }
        run(self.http.delete_invite(code).reason(reason.as_str())).await
    }
}
