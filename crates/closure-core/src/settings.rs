use std::sync::Arc;
use std::time::Duration;

use crate::capability::CapabilitySet;
use crate::event::{ChannelId, GuildId, RoleId};
use crate::locale::Locale;
use crate::module::ModuleId;

/// The effective settings of one module in one guild.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleSettings {
    pub enabled: bool,
    pub config: Arc<serde_json::Value>,
    /// Capabilities the guild allows the module to use. Never exceeds what the module declared.
    pub granted: CapabilitySet,
}

/// Guild-wide settings every module may rely on, independent of its own configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuildPolicy {
    /// Where moderation logs go, if the guild chose a channel.
    pub log_channel: Option<ChannelId>,
    /// Roles whose members may use moderation features.
    pub moderator_roles: Vec<RoleId>,
}

impl GuildPolicy {
    /// Whether a member holding `roles` counts as a moderator of the guild.
    #[must_use]
    pub fn is_moderator(&self, roles: &[RoleId]) -> bool {
        roles.iter().any(|role| self.moderator_roles.contains(role))
    }
}

/// Resource limits the event dispatcher applies to one guild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeLimits {
    /// How long a module may spend on one event before it is abandoned.
    pub handler_timeout: Duration,
    /// Events the guild may have waiting or running at once; beyond this new events are
    /// dropped and counted. Slash commands have a budget of their own, so a flood of ordinary
    /// events cannot crowd them out.
    pub queue_capacity: usize,
    /// Independent lanes for ordinary events. Events keep their order within one lane, and
    /// lanes run in parallel; an event's lane is chosen from the channel, user or role it
    /// concerns. A quarter as many extra lanes (at least one) serve slash commands.
    pub concurrency: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            handler_timeout: Duration::from_secs(10),
            queue_capacity: 256,
            concurrency: 8,
        }
    }
}

/// Source of resolved per-guild settings.
///
/// Lookups sit on the event hot path and must not block or perform I/O: implementations serve
/// them from an in-memory snapshot that is refreshed in the background.
pub trait SettingsProvider: Send + Sync {
    /// The language of the guild: its configured override, else its Discord community locale,
    /// else [`Locale::DEFAULT`].
    fn locale(&self, guild: GuildId) -> Locale;

    fn limits(&self, guild: GuildId) -> RuntimeLimits;

    fn policy(&self, guild: GuildId) -> Arc<GuildPolicy>;

    /// `None` when the guild has no entry for the module (treated as the module's defaults by
    /// the caller, never as an error).
    fn module(&self, guild: GuildId, module: &ModuleId) -> Option<ModuleSettings>;
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    #[test]
    fn moderators_are_members_holding_any_moderator_role() {
        let policy = GuildPolicy {
            log_channel: None,
            moderator_roles: vec![Id::new(5), Id::new(6)],
        };
        assert!(policy.is_moderator(&[Id::new(1), Id::new(6)]));
        assert!(!policy.is_moderator(&[Id::new(1), Id::new(2)]));
        assert!(!policy.is_moderator(&[]));
        assert!(!GuildPolicy::default().is_moderator(&[Id::new(5)]));
    }
}
