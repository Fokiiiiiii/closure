use closure_core::action::{Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::{GuildId, RoleId, UserId};
use twilight_http::request::AuditLogReason;
use twilight_model::util::Timestamp;

use super::{TwilightApi, read, read_list, run};

impl TwilightApi {
    pub(super) async fn get_guild(&self, guild: GuildId) -> Result<Outcome, DiscordError> {
        read(self.http.guild(guild))
            .await
            .map(|guild| Outcome::Guild(Box::new(guild)))
    }

    pub(super) async fn get_member(
        &self,
        guild: GuildId,
        user: UserId,
    ) -> Result<Outcome, DiscordError> {
        read(self.http.guild_member(guild, user))
            .await
            .map(|member| Outcome::Member(Box::new(member)))
    }

    pub(super) async fn kick(
        &self,
        guild: GuildId,
        user: UserId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .remove_guild_member(guild, user)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn ban(
        &self,
        guild: GuildId,
        user: UserId,
        delete_message_seconds: u32,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .create_ban(guild, user)
            .delete_message_seconds(delete_message_seconds)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn unban(
        &self,
        guild: GuildId,
        user: UserId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self.http.delete_ban(guild, user).reason(reason.as_str())).await
    }

    /// `None` lifts an existing timeout.
    pub(super) async fn timeout(
        &self,
        guild: GuildId,
        user: UserId,
        duration_seconds: Option<u32>,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let until = duration_seconds
            .map(|seconds| {
                Timestamp::from_secs((self.clock)() + i64::from(seconds))
                    .map_err(|error| DiscordError::Invalid(error.to_string()))
            })
            .transpose()?;
        run(self
            .http
            .update_guild_member(guild, user)
            .communication_disabled_until(until)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn set_voice_mute(
        &self,
        guild: GuildId,
        user: UserId,
        mute: bool,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .update_guild_member(guild, user)
            .mute(mute)
            .reason(reason.as_str()))
        .await
    }

    /// `None` resets the nickname.
    pub(super) async fn set_nickname(
        &self,
        guild: GuildId,
        user: UserId,
        nickname: Option<&str>,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .update_guild_member(guild, user)
            .nick(nickname)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn add_role(
        &self,
        guild: GuildId,
        user: UserId,
        role: RoleId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .add_guild_member_role(guild, user, role)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn remove_role(
        &self,
        guild: GuildId,
        user: UserId,
        role: RoleId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self
            .http
            .remove_guild_member_role(guild, user, role)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn list_bans(
        &self,
        guild: GuildId,
        after: Option<UserId>,
        limit: u16,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.bans(guild).limit(limit);
        if let Some(after) = after {
            request = request.after(after);
        }
        read_list(request).await.map(Outcome::Bans)
    }

    /// Refuses unless `user` is a member of `guild`: the cache is trusted when it knows, otherwise
    /// Discord is asked.
    pub(super) async fn ensure_member(
        &self,
        guild: GuildId,
        user: UserId,
    ) -> Result<(), DiscordError> {
        let known = self
            .cache
            .as_ref()
            .is_some_and(|cache| cache.member_roles(guild, user).is_some());
        if !known {
            self.get_member(guild, user).await?;
        }
        Ok(())
    }

    pub(super) async fn search_members(
        &self,
        guild: GuildId,
        query: &str,
        limit: u16,
    ) -> Result<Outcome, DiscordError> {
        read_list(self.http.search_guild_members(guild, query).limit(limit))
            .await
            .map(Outcome::Members)
    }

    pub(super) async fn list_members(
        &self,
        guild: GuildId,
        after: Option<UserId>,
        limit: u16,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.guild_members(guild).limit(limit);
        if let Some(after) = after {
            request = request.after(after);
        }
        read_list(request).await.map(Outcome::Members)
    }

    pub(super) async fn prune_count(
        &self,
        guild: GuildId,
        days: u16,
        include_roles: &[RoleId],
    ) -> Result<Outcome, DiscordError> {
        read(
            self.http
                .guild_prune_count(guild)
                .days(days)
                .include_roles(include_roles),
        )
        .await
        .map(|prune| Outcome::Count(prune.pruned))
    }

    /// Discord counts the members it removes only when asked, so the count is always asked for.
    pub(super) async fn prune(
        &self,
        guild: GuildId,
        days: u16,
        include_roles: &[RoleId],
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        read(
            self.http
                .create_guild_prune(guild)
                .days(days)
                .compute_prune_count(true)
                .include_roles(include_roles)
                .reason(reason.as_str()),
        )
        .await
        .map(|prune| Outcome::Count(prune.pruned))
    }

    pub(super) async fn create_direct_message_channel(
        &self,
        guild: GuildId,
        user: UserId,
    ) -> Result<Outcome, DiscordError> {
        self.ensure_member(guild, user).await?;
        read(self.http.create_private_channel(user))
            .await
            .map(|channel| Outcome::Channel(Box::new(channel)))
    }
}
