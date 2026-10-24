use std::collections::{BTreeMap, HashMap};

use closure_core::capability::CapabilitySet;
use closure_core::event::{ChannelId, RoleId};
use closure_core::locale::Locale;
use closure_core::module::ModuleId;
use closure_core::settings::ModuleSettings;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Issue;

/// The configuration schema version this build reads and writes.
pub const CURRENT_VERSION: u32 = 1;

/// A starting point sized for the server. It only chooses defaults; every value can still be
/// overridden individually.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Small,
    #[default]
    Medium,
    Large,
}

impl Preset {
    #[must_use]
    pub const fn limits(self) -> Limits {
        match self {
            Self::Small => Limits {
                handler_timeout_secs: 10,
                queue_capacity: 64,
                concurrency: 4,
            },
            Self::Medium => Limits {
                handler_timeout_secs: 10,
                queue_capacity: 256,
                concurrency: 8,
            },
            Self::Large => Limits {
                handler_timeout_secs: 15,
                queue_capacity: 1024,
                concurrency: 32,
            },
        }
    }
}

/// The language of messages the bot sends in a guild: `auto` follows the community locale the
/// server chose in Discord, otherwise a locale tag such as `en` or `ja`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
#[schemars(with = "String")]
pub struct Language(pub Option<Locale>);

impl TryFrom<String> for Language {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.eq_ignore_ascii_case("auto") {
            return Ok(Self(None));
        }
        value
            .parse::<Locale>()
            .map(|locale| Self(Some(locale)))
            .map_err(|_| {
                let tags: Vec<&str> = Locale::ALL.iter().map(|locale| locale.tag()).collect();
                format!(
                    "unknown language `{value}`; expected `auto` or one of: {}",
                    tags.join(", ")
                )
            })
    }
}

impl From<Language> for String {
    fn from(language: Language) -> Self {
        language
            .0
            .map_or_else(|| "auto".to_owned(), |locale| locale.tag().to_owned())
    }
}

/// Per-guild resource limits enforced by the event dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// How long one module may spend on one event before it is abandoned.
    pub handler_timeout_secs: u64,
    /// Events a guild may have waiting; beyond this the oldest work is shed and counted.
    pub queue_capacity: usize,
    /// Events of one guild handled at the same time.
    pub concurrency: usize,
}

impl Limits {
    pub(crate) fn validate(&self, issues: &mut Vec<Issue>) {
        let mut check = |path: &str, value: u64, min: u64, max: u64| {
            if !(min..=max).contains(&value) {
                issues.push(Issue::new(
                    format!("limits.{path}"),
                    "out_of_range",
                    format!("must be between {min} and {max}, got {value}"),
                ));
            }
        };
        check("handler_timeout_secs", self.handler_timeout_secs, 1, 60);
        check("queue_capacity", self.queue_capacity as u64, 8, 10_000);
        check("concurrency", self.concurrency as u64, 1, 256);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LimitsOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_capacity: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<usize>,
}

impl LimitsOverride {
    pub(crate) fn apply_to(&self, limits: &mut Limits) {
        if let Some(value) = self.handler_timeout_secs {
            limits.handler_timeout_secs = value;
        }
        if let Some(value) = self.queue_capacity {
            limits.queue_capacity = value;
        }
        if let Some(value) = self.concurrency {
            limits.concurrency = value;
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModuleOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Narrows what the module may do. It can never exceed what the module declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Vec<String>>")]
    pub capabilities: Option<CapabilitySet>,
    /// Module specific settings, validated against the module's own schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// The sparse document an administrator writes: only what differs from the defaults. This is
/// what is stored, edited by the dashboard form and exchanged as YAML.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuildOverrides {
    /// Schema version of this document; omitted means the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<Preset>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// Channel where the bot posts moderation logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub log_channel: Option<ChannelId>,
    /// Roles whose members may use moderation commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Vec<String>>")]
    pub moderator_roles: Option<Vec<RoleId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<LimitsOverride>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modules: BTreeMap<String, ModuleOverride>,
}

/// One module's settings after defaults and overrides are combined.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModuleEffective {
    pub enabled: bool,
    pub capabilities: CapabilitySet,
    pub config: serde_json::Value,
}

/// The complete configuration of a guild with nothing left implicit; what the bot acts on and
/// what the advanced editor shows as "everything".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectiveConfig {
    pub version: u32,
    pub preset: Preset,
    pub language: Language,
    pub log_channel: Option<ChannelId>,
    pub moderator_roles: Vec<RoleId>,
    pub limits: Limits,
    pub modules: BTreeMap<String, ModuleEffective>,
}

/// A validated configuration, ready for the hot path.
#[derive(Debug, Clone)]
pub struct GuildSettings {
    pub effective: EffectiveConfig,
    pub(crate) modules: HashMap<ModuleId, ModuleSettings>,
}

impl GuildSettings {
    #[must_use]
    pub fn module(&self, id: &ModuleId) -> Option<&ModuleSettings> {
        self.modules.get(id)
    }

    /// The guild's own language choice; `None` means "follow Discord".
    #[must_use]
    pub fn language_override(&self) -> Option<Locale> {
        self.effective.language.0
    }
}
