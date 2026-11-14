use closure_core::action::{NewRole, Outcome, Reason, RolePatch};
use closure_core::error::DiscordError;
use closure_core::event::{GuildId, RoleId};
use twilight_http::request::AuditLogReason;
use twilight_model::guild::{RoleColors, RolePosition};

use super::{TwilightApi, read, read_list, run};

impl TwilightApi {
    pub(super) async fn list_roles(&self, guild: GuildId) -> Result<Outcome, DiscordError> {
        read_list(self.http.roles(guild)).await.map(Outcome::Roles)
    }

    pub(super) async fn create_role(
        &self,
        guild: GuildId,
        spec: &NewRole,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let extended = spec.secondary_color.is_some() || spec.tertiary_color.is_some();
        let made = self.make_role(guild, spec, reason, extended).await;
        // A server without enhanced role colours refuses the extra colours: the role is made with
        // its main colour then, rather than not at all.
        match made {
            Err(DiscordError::Invalid(_)) if extended => {
                self.make_role(guild, spec, reason, false).await
            }
            made => made,
        }
    }

    async fn make_role(
        &self,
        guild: GuildId,
        spec: &NewRole,
        reason: &Reason,
        extended: bool,
    ) -> Result<Outcome, DiscordError> {
        let request = self
            .http
            .create_role(guild)
            .name(&spec.name)
            .permissions(spec.permissions)
            .hoist(spec.hoist)
            .mentionable(spec.mentionable)
            .reason(reason.as_str());
        let request = if extended {
            request.colors(RoleColors {
                primary_color: spec.color,
                secondary_color: spec.secondary_color,
                tertiary_color: spec.tertiary_color,
            })
        } else {
            request.color(spec.color)
        };
        read(request)
            .await
            .map(|role| Outcome::Role(Box::new(role)))
    }

    pub(super) async fn edit_role(
        &self,
        guild: GuildId,
        role: RoleId,
        patch: &RolePatch,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.update_role(guild, role).reason(reason.as_str());
        if let Some(name) = &patch.name {
            request = request.name(Some(name));
        }
        if let Some(color) = patch.color {
            request = request.color(Some(color));
        }
        if let Some(permissions) = patch.permissions {
            request = request.permissions(permissions);
        }
        if let Some(hoist) = patch.hoist {
            request = request.hoist(hoist);
        }
        if let Some(mentionable) = patch.mentionable {
            request = request.mentionable(mentionable);
        }
        read(request)
            .await
            .map(|role| Outcome::Role(Box::new(role)))
    }

    pub(super) async fn delete_role(
        &self,
        guild: GuildId,
        role: RoleId,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self.http.delete_role(guild, role).reason(reason.as_str())).await
    }

    /// Discord takes no audit log reason for this, and answers with every role of the guild.
    pub(super) async fn reorder_roles(
        &self,
        guild: GuildId,
        positions: &[RolePosition],
    ) -> Result<Outcome, DiscordError> {
        read_list(self.http.update_role_positions(guild, positions))
            .await
            .map(Outcome::Roles)
    }
}
