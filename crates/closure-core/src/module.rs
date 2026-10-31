use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use twilight_model::gateway::Intents;
use twilight_model::guild::Permissions;

use crate::capability::{Capability, CapabilitySet};
use crate::cases::Cases;
use crate::client::ApiClient;
use crate::error::{CoreError, ModuleError};
use crate::event::{CommandInvocation, Event, EventKind, EventKinds, GuildId, UserId};
use crate::history::MessageHistory;
use crate::latency::{GatewayLatency, LatencySummary};
use crate::locale::{ArgValue, Args, Catalog, Locale, Translator};
use crate::permission::{GuildPermissions, PermissionSpec};
use crate::schedule::Scheduler;
use crate::settings::GuildPolicy;
use crate::storage::Store;
use crate::subscription::{Flow, Subscription};

/// The id of the state that the modules of a bot share, such as the roles a restriction took off a
/// member. No module can have it (see [`ModuleId::restrictions`]).
const RESTRICTIONS_ID: &str = "restrictions";

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
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            && id != RESTRICTIONS_ID;
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

    /// The identity of the state that the modules of a bot share, which is not a module.
    #[must_use]
    pub fn restrictions() -> Self {
        Self(RESTRICTIONS_ID.to_owned())
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
    /// A member or a role.
    Mentionable,
    /// A file uploaded with the command.
    Attachment,
}

/// One value the user may pick for an option.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionChoice {
    /// What the user sees (1-100 characters).
    pub name: String,
    pub value: ChoiceValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChoiceValue {
    String(String),
    Integer(i64),
    Number(f64),
}

impl From<&str> for ChoiceValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<String> for ChoiceValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<i64> for ChoiceValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<f64> for ChoiceValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionSpec {
    pub name: String,
    /// The description Discord shows, resolved from the module's catalog when the command is built.
    pub kind: OptionKind,
    pub required: bool,
    /// The only values the user may pick (strings, integers and numbers; at most 25).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<OptionChoice>,
    /// Smallest value of an integer or number option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_value: Option<f64>,
    /// Largest value of an integer or number option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_value: Option<f64>,
    /// Fewest characters of a string option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<u16>,
    /// Most characters of a string option (at most 6000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<u16>,
    /// Discord asks the module for choices while the user types (see
    /// [`crate::action::InteractionResponse::Autocomplete`]). Only for strings, integers and
    /// numbers without fixed choices.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub autocomplete: bool,
}

impl OptionSpec {
    /// An optional option without further limits.
    #[must_use]
    pub fn new(name: impl Into<String>, kind: OptionKind) -> Self {
        Self {
            name: name.into(),
            kind,
            required: false,
            choices: Vec::new(),
            min_value: None,
            max_value: None,
            min_length: None,
            max_length: None,
            autocomplete: false,
        }
    }

    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Whether the limits fit the kind: choices only for strings and numbers (and then nothing
    /// else), a range only for numbers, lengths only for strings, each in order and in bounds.
    fn limits_ok(&self) -> bool {
        let text = self.kind == OptionKind::String;
        let number = matches!(self.kind, OptionKind::Integer | OptionKind::Number);
        let choices_ok = self.choices.len() <= 25
            && self.choices.iter().all(|choice| {
                (1..=100).contains(&choice.name.chars().count())
                    && match (&choice.value, self.kind) {
                        (ChoiceValue::String(value), OptionKind::String) => {
                            (1..=100).contains(&value.chars().count())
                        }
                        (ChoiceValue::Integer(_), OptionKind::Integer)
                        | (ChoiceValue::Number(_), OptionKind::Number) => true,
                        _ => false,
                    }
            });
        let range = self.min_value.is_some() || self.max_value.is_some();
        let length = self.min_length.is_some() || self.max_length.is_some();
        let autocomplete_ok = !self.autocomplete
            || (matches!(
                self.kind,
                OptionKind::String | OptionKind::Integer | OptionKind::Number
            ) && self.choices.is_empty());
        autocomplete_ok
            && choices_ok
            && (self.choices.is_empty() || !(range || length))
            && (!range || number)
            && (!length || text)
            && self
                .min_value
                .zip(self.max_value)
                .is_none_or(|(min, max)| min <= max)
            && self
                .min_length
                .zip(self.max_length)
                .is_none_or(|(min, max)| min <= max)
            && self.max_length.is_none_or(|max| (1..=6000).contains(&max))
            && self.min_length.is_none_or(|min| min <= 6000)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubcommandSpec {
    pub name: String,
    pub options: Vec<OptionSpec>,
    /// Non-empty for a subcommand group: `/command group sub`. The subcommands of a group have
    /// options of their own and no further subcommands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subcommands: Vec<SubcommandSpec>,
}

/// Discord rejects a command whose optional options precede a required one.
fn required_first(options: &[OptionSpec]) -> bool {
    let first_optional = options.iter().position(|option| !option.required);
    first_optional.is_none_or(|index| options[index..].iter().all(|option| !option.required))
}

/// What a context-menu command is used on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    User,
    Message,
}

/// A context-menu command, as data. Discord publishes its name only, without a description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCommandSpec {
    pub name: String,
    pub kind: ContextKind,
}

/// A slash command, as data.
///
/// Localised names and descriptions are not stored here: for command `ping` the catalog holds
/// `command-ping` (the name) with a `.description` attribute, per locale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub name: String,
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

        if !name_ok(&self.name) {
            return Err(invalid("name must be 1-32 chars of [a-z0-9_-]"));
        }
        if !self.options.is_empty() && !self.subcommands.is_empty() {
            return Err(invalid(
                "a command has either options or subcommands, not both",
            ));
        }
        // A name is used once among a command's subcommands and groups, so a command with both
        // kinds cannot publish two of the same name.
        let mut names = BTreeSet::new();
        if !self
            .subcommands
            .iter()
            .all(|sub| names.insert(sub.name.as_str()))
        {
            return Err(invalid("a subcommand or group name is used twice"));
        }
        if self.options.len() > 25 || self.subcommands.len() > 25 {
            return Err(invalid("at most 25 options or subcommands"));
        }
        let option_ok = |option: &OptionSpec| name_ok(&option.name) && option.limits_ok();
        if !self.options.iter().all(option_ok) {
            return Err(invalid("an option has an invalid name or limit"));
        }
        if !required_first(&self.options) {
            return Err(invalid("required options must come before optional ones"));
        }
        for sub in &self.subcommands {
            if !name_ok(&sub.name) || sub.options.len() > 25 || sub.subcommands.len() > 25 {
                return Err(invalid("a subcommand is invalid"));
            }
            if !sub.subcommands.is_empty() && !sub.options.is_empty() {
                return Err(invalid("a subcommand group has no options of its own"));
            }
            if !sub.options.iter().all(option_ok) {
                return Err(invalid("a subcommand option is invalid"));
            }
            if !required_first(&sub.options) {
                return Err(invalid("required options must come before optional ones"));
            }
            // Discord has two levels below a command: groups, then subcommands of a group.
            let mut leaves = BTreeSet::new();
            for leaf in &sub.subcommands {
                if !leaves.insert(leaf.name.as_str()) {
                    return Err(invalid("a subcommand of a group is named twice"));
                }
                if !name_ok(&leaf.name) || leaf.options.len() > 25 || !leaf.subcommands.is_empty() {
                    return Err(invalid("a subcommand of a group is invalid"));
                }
                if !leaf.options.iter().all(option_ok) {
                    return Err(invalid("a subcommand option is invalid"));
                }
                if !required_first(&leaf.options) {
                    return Err(invalid("required options must come before optional ones"));
                }
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
    /// Context-menu commands; Discord allows five per kind for each application.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_commands: Vec<ContextCommandSpec>,
    /// What the module lets a guild decide who may do; every key starts with the module's id.
    #[serde(default)]
    pub permissions: Vec<PermissionSpec>,
    /// Filters and priorities for some of `events`; an event kind without an entry here is
    /// delivered whole and at priority 0.
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
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
            context_commands: Vec::new(),
            permissions: Vec::new(),
            subscriptions: Vec::new(),
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

    /// A context-menu command on a user or on a message.
    #[must_use]
    pub fn context_command(mut self, command: ContextCommandSpec) -> Self {
        self.context_commands.push(command);
        self
    }

    /// Subscribes to an event kind with a filter and a priority (and so adds it to `events`).
    #[must_use]
    pub fn subscribe(mut self, subscription: Subscription) -> Self {
        self.events.insert(subscription.kind);
        self.subscriptions.push(subscription);
        self
    }

    /// The subscription for `kind`: the declared one, or the plain one.
    #[must_use]
    pub fn subscription(&self, kind: EventKind) -> Option<Subscription> {
        self.events.contains(&kind).then(|| {
            self.subscriptions
                .iter()
                .find(|subscription| subscription.kind == kind)
                .cloned()
                .unwrap_or_else(|| Subscription::new(kind))
        })
    }

    #[must_use]
    pub fn permission(mut self, permission: PermissionSpec) -> Self {
        self.permissions.push(permission);
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
        let mut declared = BTreeSet::new();
        for spec in &self.context_commands {
            let invalid = |reason: &str| CoreError::InvalidCommand {
                module: self.id.clone(),
                command: spec.name.clone(),
                reason: reason.to_owned(),
            };
            if !(1..=32).contains(&spec.name.chars().count()) {
                return Err(invalid("a context command name must be 1-32 characters"));
            }
            if !declared.insert((spec.kind, spec.name.clone())) {
                return Err(invalid("is declared twice"));
            }
        }
        for kind in [ContextKind::User, ContextKind::Message] {
            if self
                .context_commands
                .iter()
                .filter(|spec| spec.kind == kind)
                .count()
                > 5
            {
                return Err(CoreError::InvalidCommand {
                    module: self.id.clone(),
                    command: format!("{kind:?}"),
                    reason: "at most 5 user and 5 message commands are allowed per module".into(),
                });
            }
        }
        let mut kinds = BTreeSet::new();
        for subscription in &self.subscriptions {
            let invalid = |reason: &str| CoreError::InvalidSubscription {
                module: self.id.clone(),
                kind: subscription.kind.as_str().to_owned(),
                reason: reason.to_owned(),
            };
            if !self.events.contains(&subscription.kind) {
                return Err(invalid(
                    "the module does not subscribe to this kind of event",
                ));
            }
            if !kinds.insert(subscription.kind) {
                return Err(invalid("is declared twice"));
            }
            if !subscription.filter.fits_limits() {
                return Err(invalid("a filter lists too many entries"));
            }
        }
        let mut seen = BTreeSet::new();
        for permission in &self.permissions {
            let key = permission.key.as_str();
            if permission.key.module() != self.id.as_str() {
                return Err(CoreError::PermissionOutsideModule {
                    module: self.id.clone(),
                    key: key.to_owned(),
                });
            }
            if !seen.insert(key) {
                return Err(CoreError::DuplicatePermission {
                    module: self.id.clone(),
                    key: key.to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// The key of a module's configuration in which the administrator replaces the module's texts:
/// message id to text. A value that is not a non-empty string is ignored; see [`EventContext::t_in`].
pub const TEXTS_KEY: &str = "texts";

/// The longest text an administrator may put in place of a module's: Discord's limit for a
/// message's content, which a direct message takes.
const TEXT_LIMIT: usize = 2_000;

/// Takes the administrator's texts out of a module's configuration, so that no module reads them
/// as settings. Only a non-empty string is a text; anything else under the key is ignored.
fn split_texts(
    config: Arc<serde_json::Value>,
) -> (Arc<serde_json::Value>, HashMap<String, String>) {
    if config.get(TEXTS_KEY).is_none() {
        return (config, HashMap::new());
    }
    let texts = config
        .get(TEXTS_KEY)
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(id, text)| {
            let text = text.as_str().filter(|text| !text.is_empty())?;
            Some((id.clone(), text.to_owned()))
        })
        .collect();
    let mut settings = (*config).clone();
    if let Some(object) = settings.as_object_mut() {
        object.remove(TEXTS_KEY);
    }
    (Arc::new(settings), texts)
}

/// Puts the arguments into an administrator's text: `{name}` becomes the argument `name`, and any
/// other `{..}` stays as it is. The result is cut to [`TEXT_LIMIT`] characters, ending with `…`.
fn fill_text(template: &str, args: &Args) -> String {
    let mut filled = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        filled.push_str(&rest[..open]);
        rest = &rest[open..];
        let Some(close) = rest.find('}') else {
            break;
        };
        match args.iter().find(|(name, _)| *name == &rest[1..close]) {
            Some((_, ArgValue::Text(text))) => filled.push_str(text),
            Some((_, ArgValue::Number(number))) => filled.push_str(&number.to_string()),
            None => filled.push_str(&rest[..=close]),
        }
        rest = &rest[close + 1..];
    }
    filled.push_str(rest);
    if filled.chars().count() <= TEXT_LIMIT {
        return filled;
    }
    let mut cut: String = filled.chars().take(TEXT_LIMIT - 1).collect();
    cut.push('…');
    cut
}

/// What a module sees while handling one event.
#[derive(Clone)]
pub struct EventContext {
    pub module: ModuleId,
    pub guild_id: GuildId,
    pub api: ApiClient,
    /// The guild's language.
    pub locale: Locale,
    policy: Arc<GuildPolicy>,
    permissions: Arc<GuildPermissions>,
    store: Store,
    restrictions: Store,
    scheduler: Scheduler,
    cases: Cases,
    history: MessageHistory,
    config: Arc<serde_json::Value>,
    translator: Arc<dyn Translator>,
    texts: Arc<HashMap<String, String>>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    bot: Option<UserId>,
    gateway: Arc<GatewayLatency>,
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
        let (config, texts) = split_texts(config);
        Self {
            store: Store::unavailable(module.clone(), guild_id),
            restrictions: Store::unavailable(ModuleId::restrictions(), guild_id),
            scheduler: Scheduler::unavailable(module.clone(), guild_id),
            cases: Cases::unavailable(module.clone(), guild_id),
            history: MessageHistory::unavailable(module.clone(), guild_id),
            module,
            guild_id,
            api,
            locale,
            policy,
            permissions: Arc::default(),
            config,
            translator,
            texts: Arc::new(texts),
            clock: Arc::new(system_unix_now),
            bot: None,
            gateway: Arc::default(),
        }
    }

    /// Gives the context the Gateway's heartbeat round trips; without it nothing is measured.
    #[must_use]
    pub fn with_gateway_latency(mut self, latency: Arc<GatewayLatency>) -> Self {
        self.gateway = latency;
        self
    }

    /// The heartbeat round trips of the Gateway, or `None` before the first one.
    #[must_use]
    pub fn gateway_latency(&self) -> Option<LatencySummary> {
        self.gateway.summary()
    }

    /// Gives the context the bot's own user id, which is its application id.
    #[must_use]
    pub fn with_bot(mut self, bot: UserId) -> Self {
        self.bot = Some(bot);
        self
    }

    /// The bot's own user id, for a module that acts as the bot (automod has no moderator).
    #[must_use]
    pub fn bot(&self) -> Option<UserId> {
        self.bot
    }

    /// Replaces the clock the module reads with [`Self::now`]; for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// The time now, in Unix seconds: when a warning was given, when it expires.
    #[must_use]
    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    /// Gives the context the module's storage in this guild; without it every call fails.
    #[must_use]
    pub fn with_store(mut self, store: Store) -> Self {
        self.store = store;
        self
    }

    /// The module's own data in this guild: entries and counters.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Gives the context the state that the modules share in this guild (see
    /// [`ModuleId::restrictions`]); without it every call fails.
    #[must_use]
    pub fn with_restrictions(mut self, store: Store) -> Self {
        self.restrictions = store;
        self
    }

    /// The state that the modules of a bot share in this guild, for the punishments of the SDK.
    #[must_use]
    pub fn restrictions(&self) -> &Store {
        &self.restrictions
    }

    /// Gives the context the module's scheduler in this guild; without it every call fails.
    #[must_use]
    pub fn with_scheduler(mut self, scheduler: Scheduler) -> Self {
        self.scheduler = scheduler;
        self
    }

    /// Work the module has done later: one time or again and again.
    #[must_use]
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Gives the context the module's cases in this guild; without it every call fails.
    #[must_use]
    pub fn with_cases(mut self, cases: Cases) -> Self {
        self.cases = cases;
        self
    }

    /// The moderation cases of this guild, numbered here and shared by the modules.
    #[must_use]
    pub fn cases(&self) -> &Cases {
        &self.cases
    }

    /// Gives the context the module's message history in this guild; without it every call fails.
    #[must_use]
    pub fn with_history(mut self, history: MessageHistory) -> Self {
        self.history = history;
        self
    }

    /// The messages the host kept in this guild, if the module holds `message_history`.
    #[must_use]
    pub fn history(&self) -> &MessageHistory {
        &self.history
    }

    /// Gives the context the guild's permissions; without them nobody but the owner has any key.
    #[must_use]
    pub fn with_permissions(mut self, permissions: Arc<GuildPermissions>) -> Self {
        self.permissions = permissions;
        self
    }

    /// Who may do what in this guild.
    #[must_use]
    pub fn permissions(&self) -> &GuildPermissions {
        &self.permissions
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

    /// The translator, for messages that belong to no module (see [`Self::t_core`]).
    #[must_use]
    pub fn translator(&self) -> &dyn Translator {
        self.translator.as_ref()
    }

    /// A message of the catalog every module shares (`core`), such as the SDK's own.
    #[must_use]
    pub fn t_core(&self, locale: Locale, key: &str, args: &Args) -> String {
        self.translate_in(locale, "core", key, args)
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
        self.translate_in(locale, self.module.as_str(), key, args)
    }

    /// The administrator's text for `key` ([`TEXTS_KEY`]) when there is one, else the catalog's.
    fn translate_in(&self, locale: Locale, namespace: &str, key: &str, args: &Args) -> String {
        match self.texts.get(key) {
            Some(template) => fill_text(template, args),
            None => self.translator.translate(locale, namespace, key, args),
        }
    }

    /// The language to answer a slash command in: the invoker's own, else the guild's.
    #[must_use]
    pub fn reply_locale(&self, invocation: &CommandInvocation) -> Locale {
        invocation.locale.unwrap_or(self.locale)
    }
}

/// The wall clock in Unix seconds, the default of [`EventContext::now`].
fn system_unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
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

    /// [`Self::on_event`] that also says whether the modules with a lower priority get the
    /// event. The dispatcher calls this one; a module that never stops an event leaves it be.
    async fn on_event_flow(&self, ctx: &EventContext, event: &Event) -> Result<Flow, ModuleError> {
        self.on_event(ctx, event).await.map(|()| Flow::Continue)
    }

    /// Called once during graceful shutdown.
    async fn on_unload(&self) {}
}

/// The text of the entry `key` of a Fluent source, when it is one line: `key = text`.
/// Fluent writes a brace as `{"{"}` or `{"}"}`; the text comes back with plain braces.
#[must_use]
pub fn catalog_text(source: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} = ");
    source
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|text| text.replace("{\"{\"}", "{").replace("{\"}\"}", "}"))
}

/// Replaces each description of a JSON schema that names a catalog entry (`"config-…"`) by the text
/// `lookup` gives for it. A key without a text stays as it is.
pub fn resolve_descriptions(
    schema: &mut serde_json::Value,
    lookup: &dyn Fn(&str) -> Option<String>,
) {
    match schema {
        serde_json::Value::Object(map) => {
            let key = map
                .get("description")
                .and_then(serde_json::Value::as_str)
                .filter(|key| key.starts_with("config-"))
                .map(str::to_owned);
            if let Some(text) = key.and_then(|key| lookup(&key)) {
                map.insert("description".to_owned(), serde_json::Value::String(text));
            }
            for value in map.values_mut() {
                resolve_descriptions(value, lookup);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                resolve_descriptions(item, lookup);
            }
        }
        _ => {}
    }
}

/// Gives the descriptions of a schema their text in `locale`: the module's own catalog in that locale, then
/// its default catalog, then the shared `core` text. A key with no text stays as it is.
pub fn localize_descriptions(
    schema: &mut serde_json::Value,
    catalogs: &[Catalog],
    locale: Locale,
    core: &dyn Fn(&str) -> Option<String>,
) {
    let text_in = |wanted: Locale, key: &str| {
        catalogs
            .iter()
            .find(|catalog| catalog.locale == wanted)
            .and_then(|catalog| catalog_text(&catalog.source, key))
    };
    resolve_descriptions(schema, &|key| {
        text_in(locale, key)
            .or_else(|| text_in(Locale::DEFAULT, key))
            .or_else(|| core(key))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(name: &str) -> SubcommandSpec {
        SubcommandSpec {
            name: name.into(),
            options: vec![],
            subcommands: vec![],
        }
    }

    fn group(name: &str, leaves: Vec<SubcommandSpec>) -> SubcommandSpec {
        SubcommandSpec {
            name: name.into(),
            options: vec![],
            subcommands: leaves,
        }
    }

    /// Whether a manifest whose only command `warn` holds `subcommands` is accepted.
    fn accepted(subcommands: Vec<SubcommandSpec>) -> bool {
        ModuleManifest::new(ModuleId::new("warn").unwrap(), "warn", "0.1.0")
            .command(CommandSpec {
                name: "warn".into(),
                options: vec![],
                subcommands,
                default_member_permissions: None,
            })
            .validate()
            .is_ok()
    }

    #[test]
    fn subcommands_and_groups_mix_in_one_command_in_either_order() {
        assert!(accepted(vec![
            leaf("status"),
            group("list", vec![leaf("all")]),
        ]));
        assert!(accepted(vec![
            group("list", vec![leaf("all")]),
            leaf("status"),
        ]));
    }

    #[test]
    fn a_name_is_used_once_among_the_subcommands_and_groups_of_a_command() {
        assert!(!accepted(vec![leaf("status"), leaf("status")]));
        assert!(!accepted(vec![
            leaf("list"),
            group("list", vec![leaf("all")]),
        ]));
        assert!(!accepted(vec![
            group("list", vec![leaf("all")]),
            group("list", vec![leaf("clear")]),
        ]));
    }

    #[test]
    fn a_name_may_repeat_in_another_group() {
        assert!(accepted(vec![
            group("list", vec![leaf("all")]),
            group("clear", vec![leaf("all")]),
        ]));
    }

    #[test]
    fn a_command_takes_at_most_25_subcommands_and_groups() {
        let many = |count: usize| {
            (0..count)
                .map(|i| leaf(&format!("s{i}")))
                .collect::<Vec<_>>()
        };
        assert!(accepted(many(25)));
        assert!(!accepted(many(26)));
    }

    #[test]
    fn catalog_text_returns_fluent_brace_literals_as_braces() {
        let source = "config-texts = Fills {\"{\"}name{\"}\"} in.\n";

        assert_eq!(
            catalog_text(source, "config-texts").as_deref(),
            Some("Fills {name} in.")
        );
        assert_eq!(catalog_text(source, "config-missing"), None);
    }
}
