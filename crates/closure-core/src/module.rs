use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use twilight_model::gateway::Intents;
use twilight_model::guild::Permissions;

use crate::capability::{Capability, CapabilitySet};
use crate::client::ApiClient;
use crate::error::{CoreError, ModuleError};
use crate::event::{CommandInvocation, Event, EventKind, EventKinds, GuildId};
use crate::locale::{Args, Catalog, Locale, Translator};
use crate::settings::GuildPolicy;

/// Stable identifier of a module, used in configuration keys, metrics and translation
/// namespaces. Restricted to `[a-z0-9_-]`, starting with a letter, at most 32 characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModuleId(String);

impl ModuleId {
    pub fn new(id: impl Into<String>) -> Result<Self, CoreError> {
        let id = id.into();
        let valid = (1..=32).contains(&id.len())
            && id.starts_with(|c: char| c.is_ascii_lowercase())
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if valid {
            Ok(Self(id))
        } else {
            Err(CoreError::InvalidModuleId(id))
        }
    }

    /// The identity of actions the host performs itself, e.g. telling a user a command failed.
    #[must_use]
    pub fn system() -> Self {
        Self("system".to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ModuleId {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ModuleId> for String {
    fn from(id: ModuleId) -> Self {
        id.0
    }
}

/// How a module is executed. Only native modules exist today; the other variants reserve
/// the slot so that loading scripts later does not change the manifest format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModuleRuntime {
    Native,
    Lua,
    Wasm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKind {
    String,
    Integer,
    Number,
    Boolean,
    User,
    Channel,
    Role,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionSpec {
    pub name: String,
    /// English description; other languages come from the catalog.
    pub description: String,
    pub kind: OptionKind,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubcommandSpec {
    pub name: String,
    pub description: String,
    pub options: Vec<OptionSpec>,
}

/// Discord rejects a command whose optional options precede a required one.
fn required_first(options: &[OptionSpec]) -> bool {
    let first_optional = options.iter().position(|option| !option.required);
    first_optional.is_none_or(|index| options[index..].iter().all(|option| !option.required))
}

/// A slash command, as data.
///
/// Localised names and descriptions are not stored here: for command `ping` the catalog holds
/// `command-ping` (the name) with a `.description` attribute, per locale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
    /// English description.
    pub description: String,
    pub options: Vec<OptionSpec>,
    pub subcommands: Vec<SubcommandSpec>,
    /// Permissions a member needs to see the command by default. `None` means everyone.
    pub default_member_permissions: Option<Permissions>,
}

impl CommandSpec {
    fn validate(&self, module: &ModuleId) -> Result<(), CoreError> {
        let invalid = |reason: &str| CoreError::InvalidCommand {
            module: module.clone(),
            command: self.name.clone(),
            reason: reason.to_owned(),
        };
        let name_ok = |name: &str| {
            (1..=32).contains(&name.len())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        };
        let text_ok = |text: &str| (1..=100).contains(&text.chars().count());

        if !name_ok(&self.name) {
            return Err(invalid("name must be 1-32 chars of [a-z0-9_-]"));
        }
        if !text_ok(&self.description) {
            return Err(invalid("description must be 1-100 characters"));
        }
        if !self.options.is_empty() && !self.subcommands.is_empty() {
            return Err(invalid(
                "a command has either options or subcommands, not both",
            ));
        }
        if self.options.len() > 25 || self.subcommands.len() > 25 {
            return Err(invalid("at most 25 options or subcommands"));
        }
        let option_ok = |option: &OptionSpec| name_ok(&option.name) && text_ok(&option.description);
        if !self.options.iter().all(option_ok) {
            return Err(invalid("an option has an invalid name or description"));
        }
        if !required_first(&self.options) {
            return Err(invalid("required options must come before optional ones"));
        }
        for sub in &self.subcommands {
            if !name_ok(&sub.name) || !text_ok(&sub.description) || sub.options.len() > 25 {
                return Err(invalid("a subcommand is invalid"));
            }
            if !sub.options.iter().all(option_ok) {
                return Err(invalid("a subcommand option is invalid"));
            }
            if !required_first(&sub.options) {
                return Err(invalid("required options must come before optional ones"));
            }
        }
        Ok(())
    }
}

/// Everything the host needs to know about a module before running it. It is plain data so
/// a Lua or WASM module can supply one from a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleManifest {
    pub id: ModuleId,
    pub name: String,
    pub version: String,
    pub description: String,
    pub runtime: ModuleRuntime,
    /// Gateway intents the module needs. The bot requests the union over all registered
    /// modules, because intents belong to the connection and cannot follow per-guild settings.
    pub intents: Intents,
    /// Events the module wants delivered.
    pub events: EventKinds,
    /// Capabilities the module needs. Native modules receive these by default.
    pub capabilities: CapabilitySet,
    pub dependencies: Vec<ModuleId>,
    pub commands: Vec<CommandSpec>,
    /// JSON Schema for the module's section of the guild configuration.
    pub config_schema: serde_json::Value,
    pub default_config: serde_json::Value,
    pub default_enabled: bool,
    pub catalogs: Vec<Catalog>,
}

impl ModuleManifest {
    #[must_use]
    pub fn new(id: ModuleId, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            version: version.into(),
            description: String::new(),
            runtime: ModuleRuntime::Native,
            intents: Intents::empty(),
            events: EventKinds::new(),
            capabilities: CapabilitySet::new(),
            dependencies: Vec::new(),
            commands: Vec::new(),
            config_schema: serde_json::json!({ "type": "object" }),
            default_config: serde_json::json!({}),
            default_enabled: false,
            catalogs: Vec::new(),
        }
    }

    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the JSON Schema of the module's configuration section and its default value.
    #[must_use]
    pub fn config(mut self, schema: serde_json::Value, default: serde_json::Value) -> Self {
        self.config_schema = schema;
        self.default_config = default;
        self
    }

    #[must_use]
    pub fn intents(mut self, intents: Intents) -> Self {
        self.intents = intents;
        self
    }

    #[must_use]
    pub fn events(mut self, events: impl IntoIterator<Item = EventKind>) -> Self {
        self.events = events.into_iter().collect();
        self
    }

    #[must_use]
    pub fn capabilities(mut self, capabilities: impl IntoIterator<Item = Capability>) -> Self {
        self.capabilities = capabilities.into_iter().collect();
        self
    }

    #[must_use]
    pub fn depends_on(mut self, module: ModuleId) -> Self {
        self.dependencies.push(module);
        self
    }

    #[must_use]
    pub fn command(mut self, command: CommandSpec) -> Self {
        self.commands.push(command);
        self
    }

    #[must_use]
    pub fn catalog(mut self, locale: Locale, source: impl Into<String>) -> Self {
        self.catalogs.push(Catalog {
            locale,
            source: source.into(),
        });
        self
    }

    #[must_use]
    pub fn enabled_by_default(mut self) -> Self {
        self.default_enabled = true;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), CoreError> {
        for command in &self.commands {
            command.validate(&self.id)?;
        }
        Ok(())
    }
}

/// What a module sees while handling one event.
pub struct EventContext {
    pub module: ModuleId,
    pub guild_id: GuildId,
    pub api: ApiClient,
    /// The guild's language.
    pub locale: Locale,
    policy: Arc<GuildPolicy>,
    config: Arc<serde_json::Value>,
    translator: Arc<dyn Translator>,
}

impl fmt::Debug for EventContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventContext")
            .field("module", &self.module)
            .field("guild_id", &self.guild_id)
            .field("locale", &self.locale)
            .finish_non_exhaustive()
    }
}

impl EventContext {
    #[must_use]
    pub fn new(
        module: ModuleId,
        guild_id: GuildId,
        api: ApiClient,
        locale: Locale,
        policy: Arc<GuildPolicy>,
        config: Arc<serde_json::Value>,
        translator: Arc<dyn Translator>,
    ) -> Self {
        Self {
            module,
            guild_id,
            api,
            locale,
            policy,
            config,
            translator,
        }
    }

    /// The guild-wide settings: log channel and moderator roles.
    #[must_use]
    pub fn policy(&self) -> &GuildPolicy {
        &self.policy
    }

    /// The effective configuration of this module in this guild.
    #[must_use]
    pub fn config(&self) -> &serde_json::Value {
        &self.config
    }

    /// Deserialises the module's configuration into its own typed struct.
    pub fn parse_config<T: DeserializeOwned>(&self) -> Result<T, ModuleError> {
        serde_json::from_value((*self.config).clone())
            .map_err(|error| ModuleError::Config(error.to_string()))
    }

    /// Translates `key` into the guild language.
    #[must_use]
    pub fn t(&self, key: &str) -> String {
        self.t_in(self.locale, key, &Args::new())
    }

    #[must_use]
    pub fn t_args(&self, key: &str, args: &Args) -> String {
        self.t_in(self.locale, key, args)
    }

    #[must_use]
    pub fn t_in(&self, locale: Locale, key: &str, args: &Args) -> String {
        self.translator
            .translate(locale, self.module.as_str(), key, args)
    }

    /// The language to answer a slash command in: the invoker's own, else the guild's.
    #[must_use]
    pub fn reply_locale(&self, invocation: &CommandInvocation) -> Locale {
        invocation.locale.unwrap_or(self.locale)
    }
}

/// A unit of bot functionality.
///
/// Implementations must be cheap to call concurrently: the dispatcher runs handlers for
/// different guilds in parallel and applies a timeout to each call.
#[async_trait]
pub trait Module: Send + Sync + 'static {
    fn manifest(&self) -> &ModuleManifest;

    /// Called once at startup, before any event is delivered.
    async fn on_load(&self) -> Result<(), ModuleError> {
        Ok(())
    }

    /// Called for each subscribed event of a guild where the module is enabled.
    async fn on_event(&self, ctx: &EventContext, event: &Event) -> Result<(), ModuleError>;

    /// Called once during graceful shutdown.
    async fn on_unload(&self) {}
}
