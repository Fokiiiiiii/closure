//! Two-tier guild configuration.
//!
//! A guild is configured by one sparse document, [`GuildOverrides`]: only what an administrator
//! wrote is stored, and everything else comes from the preset and the module manifests. The
//! *simple* tier (a dashboard form) edits a handful of fields of that document; the *advanced*
//! tier is the whole document as YAML. Both are the same schema, so switching tiers never loses
//! information and defaults can improve without rewriting stored configs.

mod compile;
mod error;
mod model;
mod schema;
mod store;
mod view;
mod yaml;

pub use compile::ConfigCompiler;
pub use error::{ConfigError, Issue};
pub use model::{
    CURRENT_VERSION, EffectiveConfig, GuildOverrides, GuildSettings, Language, Limits,
    LimitsOverride, ModuleEffective, ModuleOverride, Preset,
};
pub use schema::{document_schema, document_schema_for};
pub use store::SettingsStore;
pub use view::{BASIC_KEYS, View, basic_view, merge_basic};
pub use yaml::{effective_to_yaml, overrides_to_yaml, parse_overrides};
