use closure_core::action::{
    ChannelKind, ChannelPatch, ChannelPosition, NewChannel, Outcome, OverwriteSpec,
    OverwriteTarget, Reason,
};
use closure_core::error::DiscordError;
use closure_core::event::{ChannelId, GuildId};
use twilight_http::request::AuditLogReason;
use twilight_model::channel::ChannelType;
use twilight_model::channel::permission_overwrite::{
    PermissionOverwrite as ChannelOverwrite, PermissionOverwriteType as ChannelOverwriteType,
};
use twilight_model::http::channel_position::Position;
use twilight_model::http::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType};

use super::{TwilightApi, read, read_list, run};

fn channel_type(kind: ChannelKind) -> ChannelType {
    match kind {
        ChannelKind::Text => ChannelType::GuildText,
        ChannelKind::Voice => ChannelType::GuildVoice,
        ChannelKind::Category => ChannelType::GuildCategory,
        ChannelKind::Announcement => ChannelType::GuildAnnouncement,
        ChannelKind::Forum => ChannelType::GuildForum,
        ChannelKind::Stage => ChannelType::GuildStageVoice,
    }
}

/// The form Discord accepts when updating a single overwrite.
fn overwrite(spec: &OverwriteSpec) -> PermissionOverwrite {
    let (id, kind) = match spec.target {
        OverwriteTarget::Role(role) => (role.cast(), PermissionOverwriteType::Role),
        OverwriteTarget::Member(user) => (user.cast(), PermissionOverwriteType::Member),
    };
    PermissionOverwrite {
        allow: Some(spec.allow),
        deny: Some(spec.deny),
        id,
        kind,
    }
}

/// The form used inside a channel definition.
fn channel_overwrite(spec: &OverwriteSpec) -> ChannelOverwrite {
    let (id, kind) = match spec.target {
        OverwriteTarget::Role(role) => (role.cast(), ChannelOverwriteType::Role),
        OverwriteTarget::Member(user) => (user.cast(), ChannelOverwriteType::Member),
    };
    ChannelOverwrite {
        allow: spec.allow,
        deny: spec.deny,
        id,
        kind,
    }
}

impl TwilightApi {
    pub(super) async fn list_channels(&self, guild: GuildId) -> Result<Outcome, DiscordError> {
        read_list(self.http.guild_channels(guild))
            .await
            .map(Outcome::Channels)
    }

    pub(super) async fn get_channel(&self, channel: ChannelId) -> Result<Outcome, DiscordError> {
        read(self.http.channel(channel))
            .await
            .map(|channel| Outcome::Channel(Box::new(channel)))
    }

    pub(super) async fn create_channel(
        &self,
        guild: GuildId,
        spec: &NewChannel,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let overwrites: Vec<ChannelOverwrite> =
            spec.overwrites.iter().map(channel_overwrite).collect();
        let mut request = self
            .http
            .create_guild_channel(guild, &spec.name)
            .kind(channel_type(spec.kind))
            .nsfw(spec.nsfw)
            .rate_limit_per_user(spec.slowmode_seconds)
            .permission_overwrites(&overwrites)
            .reason(reason.as_str());
        if let Some(topic) = &spec.topic {
            request = request.topic(topic);
        }
        if let Some(parent) = spec.parent_id {
            request = request.parent_id(parent);
        }
        read(request)
            .await
            .map(|channel| Outcome::Channel(Box::new(channel)))
    }

    pub(super) async fn edit_channel(
        &self,
        channel: ChannelId,
        patch: &ChannelPatch,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.update_channel(channel).reason(reason.as_str());
        if let Some(name) = &patch.name {
            request = request.name(name);
        }
        if let Some(topic) = &patch.topic {
            request = request.topic(topic);
        }
        if let Some(nsfw) = patch.nsfw {
            request = request.nsfw(nsfw);
        }
        if let Some(seconds) = patch.slowmode_seconds {
            request = request.rate_limit_per_user(seconds);
        }
        read(request)
            .await
            .map(|channel| Outcome::Channel(Box::new(channel)))
    }

    pub(super) async fn delete_channel(
        &self,
        channel: ChannelId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self.http.delete_channel(channel).reason(reason.as_str())).await
    }

    pub(super) async fn set_permission_overwrite(
        &self,
        channel: ChannelId,
        spec: &OverwriteSpec,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let overwrite = overwrite(spec);
        run(self
            .http
            .update_channel_permission(channel, &overwrite)
            .reason(reason.as_str()))
        .await
    }

    pub(super) async fn delete_permission_overwrite(
        &self,
        channel: ChannelId,
        target: OverwriteTarget,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let request = self.http.delete_channel_permission(channel);
        let request = match target {
            OverwriteTarget::Role(role) => request.role(role),
            OverwriteTarget::Member(user) => request.member(user),
        };
        run(request.reason(reason.as_str())).await
    }

    /// Discord takes no audit log reason for this.
    pub(super) async fn reorder_channels(
        &self,
        guild: GuildId,
        positions: &[ChannelPosition],
    ) -> Result<Outcome, DiscordError> {
        let positions: Vec<Position> = positions
            .iter()
            .map(|position| Position::from((position.channel_id, position.position)))
            .collect();
        run(self.http.update_guild_channel_positions(guild, &positions)).await
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::guild::Permissions;
    use twilight_model::id::Id;

    use super::*;

    #[test]
    fn overwrites_keep_their_target_kind_and_id() {
        let role = overwrite(&OverwriteSpec {
            target: OverwriteTarget::Role(Id::new(5)),
            allow: Permissions::SEND_MESSAGES,
            deny: Permissions::empty(),
        });
        assert_eq!(role.kind, PermissionOverwriteType::Role);
        assert_eq!(role.id.get(), 5);

        let member = overwrite(&OverwriteSpec {
            target: OverwriteTarget::Member(Id::new(6)),
            allow: Permissions::empty(),
            deny: Permissions::VIEW_CHANNEL,
        });
        assert_eq!(member.kind, PermissionOverwriteType::Member);
        assert_eq!(member.id.get(), 6);
        assert_eq!(member.deny, Some(Permissions::VIEW_CHANNEL));
    }

    #[test]
    fn channel_definitions_keep_full_permission_sets() {
        let spec = OverwriteSpec {
            target: OverwriteTarget::Role(Id::new(5)),
            allow: Permissions::SEND_MESSAGES,
            deny: Permissions::VIEW_CHANNEL,
        };
        let converted = channel_overwrite(&spec);
        assert_eq!(converted.kind, ChannelOverwriteType::Role);
        assert_eq!(converted.allow, Permissions::SEND_MESSAGES);
        assert_eq!(converted.deny, Permissions::VIEW_CHANNEL);
    }

    #[test]
    fn every_channel_kind_maps_to_a_guild_channel_type() {
        assert_eq!(
            channel_type(ChannelKind::Category),
            ChannelType::GuildCategory
        );
        assert_eq!(channel_type(ChannelKind::Text), ChannelType::GuildText);
    }
}
