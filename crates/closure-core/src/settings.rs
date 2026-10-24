use std::sync::Arc;
use std::time::Duration;

use crate::capability::CapabilitySet;
use crate::event::GuildId;
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

/// Resource limits the event dispatcher applies to one guild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeLimits {
    /// How long a module may spend on one event before it is abandoned.
    pub handler_timeout: Duration,
    /// Events the guild may have waiting; beyond this new events are dropped and counted.
    pub queue_capacity: usize,
    /// Independent lanes of the guild. Events keep their order within one lane, and lanes run
    /// in parallel; an event's lane is chosen from the user or channel it concerns.
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

    /// `None` when the guild has no entry for the module (treated as the module's defaults by
    /// the caller, never as an error).
    fn module(&self, guild: GuildId, module: &ModuleId) -> Option<ModuleSettings>;
}
