use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use closure_core::capability::CapabilitySet;
use closure_core::event::{ChannelId, RoleId, UserId};
use closure_core::locale::Locale;
use closure_core::module::ModuleId;
use closure_core::permission::{GuildPermissions, Level};
use closure_core::settings::{GuildPolicy, ModuleSettings};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Issue;

/// The configuration schema version this build reads and writes.
pub const CURRENT_VERSION: u32 = 1;

/// A Discord id as the parser takes it: the digits in a string (how it is written), or a number.
fn snowflake() -> serde_json::Value {
    serde_json::json!({
        "anyOf": [
            { "type": "string", "pattern": "^\\+?[0-9]+$" },
            { "type": "integer", "minimum": 1 }
        ]
    })
}

fn optional_snowflake_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "anyOf": [snowflake(), { "type": "null" }] })
}

fn optional_snowflakes_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [{ "type": "array", "items": snowflake() }, { "type": "null" }]
    })
}

fn optional_timezone_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [{
            "type": "string",
            "description": "The zone of a date typed without one: UTC, Z, an offset such as UTC+9, UTC+09:00, UTC-5, +09:00 or -0500, or a fixed zone such as Asia/Tokyo. Zones with daylight saving time are refused; use a fixed offset instead."
        }, { "type": "null" }]
    })
}

fn optional_prefix_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [{
            "type": "string",
            "minLength": 1,
            "maxLength": 10,
            "pattern": "^\\S+$",
            "description": "The prefix of text commands, 1 to 10 characters without spaces. Unset, text commands are off."
        }, { "type": "null" }]
    })
}

fn optional_emoji_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "anyOf": [{
            "type": "string",
            "maxLength": 100,
            "description": "A Unicode emoji, or a custom one as <:name:id> or <a:name:id>. An empty string means no emoji."
        }, { "type": "null" }]
    })
}

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
    /// Events a guild may have waiting; beyond this new events are dropped and counted.
    pub queue_capacity: usize,
    /// Independent lanes of the guild: events keep their order within a lane, lanes run in
    /// parallel. An event's lane is chosen from the user or channel it concerns.
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

/// A level written as its name (`moderator`) or as a number from 1 to 100.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum LevelSetting {
    Number(u8),
    Name(String),
}

impl LevelSetting {
    pub(crate) fn resolve(&self) -> Result<Level, String> {
        match self {
            Self::Number(number) => Level::new(*number).map_err(|error| error.to_string()),
            Self::Name(name) => Level::named(name).ok_or_else(|| {
                format!(
                    "`{name}` is not a level; use a number from 1 to 100 or one of general, trusted, moderator, admin, owner"
                )
            }),
        }
    }
}

/// Roles and members, by id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Principals {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_snowflakes_schema")]
    pub roles: Option<Vec<RoleId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_snowflakes_schema")]
    pub users: Option<Vec<UserId>>,
}

/// What a guild decided about one permission key (`moderation.ban`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyOverride {
    /// The level a member needs, instead of the one the module declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level: Option<LevelSetting>,
    /// Always allowed, whatever their level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<Principals>,
    /// Never allowed (except the owner of the server); wins over `allow`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<Principals>,
}

/// Who may do what. A member's level is the highest that any of their roles, their own entry
/// or Discord's Administrator permission gives; the owner of the server is always at 100.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionsOverride {
    /// The level of a member who holds Discord's Administrator permission (default `admin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub administrator_level: Option<LevelSetting>,
    /// Role id to level.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, LevelSetting>,
    /// User id to level.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub users: BTreeMap<String, LevelSetting>,
    /// Permission key (`moderation.ban`) to what is decided about it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, KeyOverride>,
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
    #[schemars(schema_with = "optional_snowflake_schema")]
    pub log_channel: Option<ChannelId>,
    /// Roles whose members may use moderation commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_snowflakes_schema")]
    pub moderator_roles: Option<Vec<RoleId>>,
    /// Kept as written and checked when the document is compiled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_timezone_schema")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_prefix_schema")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_emoji_schema")]
    pub success_emoji: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "optional_emoji_schema")]
    pub error_emoji: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<LimitsOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionsOverride>,
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
    /// The zone as a label such as `UTC+09:00`.
    pub timezone: Option<String>,
    pub prefix: Option<String>,
    pub success_emoji: String,
    pub error_emoji: String,
    pub limits: Limits,
    pub permissions: PermissionsOverride,
    pub modules: BTreeMap<String, ModuleEffective>,
}

/// A validated configuration, ready for the hot path.
#[derive(Debug, Clone)]
pub struct GuildSettings {
    pub effective: EffectiveConfig,
    pub(crate) modules: HashMap<ModuleId, ModuleSettings>,
    pub(crate) policy: Arc<GuildPolicy>,
    pub(crate) permissions: Arc<GuildPermissions>,
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
