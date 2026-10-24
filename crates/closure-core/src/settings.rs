use std::sync::Arc;

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

/// Source of resolved per-guild settings.
///
/// Lookups sit on the event hot path and must not block or perform I/O: implementations serve
/// them from an in-memory snapshot that is refreshed in the background.
pub trait SettingsProvider: Send + Sync {
    /// The language of the guild: its configured override, else its Discord community locale,
    /// else [`Locale::DEFAULT`].
    fn locale(&self, guild: GuildId) -> Locale;

    /// `None` when the guild has no entry for the module (treated as the module's defaults by
    /// the caller, never as an error).
    fn module(&self, guild: GuildId, module: &ModuleId) -> Option<ModuleSettings>;
}
