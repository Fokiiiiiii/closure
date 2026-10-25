use std::sync::Arc;
use std::time::Duration;

use closure_core::event::GuildId;
use closure_core::locale::Locale;
use closure_core::module::ModuleId;
use closure_core::permission::GuildPermissions;
use closure_core::settings::{GuildPolicy, ModuleSettings, RuntimeLimits, SettingsProvider};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

use crate::compile::ConfigCompiler;
use crate::error::ConfigError;
use crate::model::{GuildOverrides, GuildSettings};
use crate::yaml::parse_overrides;

/// In-memory, concurrently readable settings of every guild.
///
/// Reads never block on I/O or on a writer for another guild. A configuration that fails
/// validation is rejected as a whole and the previous settings stay in force, so a bad edit can
/// never leave a guild half-configured.
pub struct SettingsStore {
    compiler: ConfigCompiler,
    guilds: DashMap<GuildId, Versioned>,
    preferred_locales: DashMap<GuildId, Locale>,
}

/// Settings together with the database revision they were read at.
struct Versioned {
    revision: i64,
    settings: Arc<GuildSettings>,
}

impl std::fmt::Debug for SettingsStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsStore")
            .field("guilds", &self.guilds.len())
            .finish_non_exhaustive()
    }
}

impl SettingsStore {
    #[must_use]
    pub fn new(compiler: ConfigCompiler) -> Self {
        Self {
            compiler,
            guilds: DashMap::new(),
            preferred_locales: DashMap::new(),
        }
    }

    #[must_use]
    pub fn compiler(&self) -> &ConfigCompiler {
        &self.compiler
    }

    /// Validates `overrides` and, only if they are valid, makes them the guild's settings,
    /// regardless of any revision. For deployments without a database and for tests; code that
    /// reads from the database uses [`install`](Self::install).
    pub fn set_guild(
        &self,
        guild: GuildId,
        overrides: &GuildOverrides,
    ) -> Result<Arc<GuildSettings>, ConfigError> {
        let settings = Arc::new(self.compiler.compile(overrides)?);
        self.guilds.insert(
            guild,
            Versioned {
                revision: 0,
                settings: Arc::clone(&settings),
            },
        );
        Ok(settings)
    }

    /// Installs settings read at `revision`, unless newer ones are already in place. Concurrent
    /// writers and the change listener may all call this; the highest revision always wins, so
    /// they can race without a stale read overwriting a fresh one. Returns whether it installed.
    #[must_use]
    pub fn install(&self, guild: GuildId, revision: i64, settings: GuildSettings) -> bool {
        let versioned = Versioned {
            revision,
            settings: Arc::new(settings),
        };
        match self.guilds.entry(guild) {
            Entry::Occupied(mut entry) if revision >= entry.get().revision => {
                entry.insert(versioned);
                true
            }
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(versioned);
                true
            }
        }
    }

    /// Parses and applies a YAML document.
    pub fn set_guild_yaml(
        &self,
        guild: GuildId,
        yaml: &str,
    ) -> Result<Arc<GuildSettings>, ConfigError> {
        self.set_guild(guild, &parse_overrides(yaml)?)
    }

    /// Forgets a guild's configuration, e.g. after the bot was removed from it.
    pub fn remove_guild(&self, guild: GuildId) {
        self.guilds.remove(&guild);
        self.preferred_locales.remove(&guild);
    }

    /// Records the community locale Discord reports for the guild (used by `language: auto`).
    pub fn set_preferred_locale(&self, guild: GuildId, locale: Option<Locale>) {
        match locale {
            Some(locale) => {
                self.preferred_locales.insert(guild, locale);
            }
            None => {
                self.preferred_locales.remove(&guild);
            }
        }
    }

    /// The guild's settings, or the defaults when it has configured nothing.
    #[must_use]
    pub fn settings(&self, guild: GuildId) -> Arc<GuildSettings> {
        self.guilds.get(&guild).map_or_else(
            || Arc::clone(self.compiler.defaults()),
            |entry| Arc::clone(&entry.settings),
        )
    }

    /// Whether the guild has a stored configuration in memory (as opposed to using defaults).
    #[must_use]
    pub fn is_configured(&self, guild: GuildId) -> bool {
        self.guilds.contains_key(&guild)
    }

    #[must_use]
    pub fn configured_guilds(&self) -> usize {
        self.guilds.len()
    }
}

impl SettingsProvider for SettingsStore {
    fn locale(&self, guild: GuildId) -> Locale {
        self.settings(guild)
            .language_override()
            .or_else(|| self.preferred_locales.get(&guild).map(|entry| *entry))
            .unwrap_or(Locale::DEFAULT)
    }

    fn limits(&self, guild: GuildId) -> RuntimeLimits {
        let limits = self.settings(guild).effective.limits;
        RuntimeLimits {
            handler_timeout: Duration::from_secs(limits.handler_timeout_secs),
            queue_capacity: limits.queue_capacity,
            concurrency: limits.concurrency,
        }
    }

    fn policy(&self, guild: GuildId) -> Arc<GuildPolicy> {
        Arc::clone(&self.settings(guild).policy)
    }

    fn permissions(&self, guild: GuildId) -> Arc<GuildPermissions> {
        Arc::clone(&self.settings(guild).permissions)
    }

    fn module(&self, guild: GuildId, module: &ModuleId) -> Option<ModuleSettings> {
        self.settings(guild).module(module).cloned()
    }
}

#[cfg(test)]
mod tests {
    use closure_core::module::{Module, ModuleManifest};
    use closure_core::registry::ModuleRegistry;
    use closure_core::testing::StubModule;
    use serde_json::json;
    use twilight_model::id::Id;

    use super::*;

    fn store() -> SettingsStore {
        let mut manifest = ModuleManifest::new(ModuleId::new("warn").unwrap(), "warn", "0.1.0");
        manifest.config_schema = json!({
            "type": "object",
            "properties": { "limit": { "type": "integer", "maximum": 10 } }
        });
        manifest.default_config = json!({ "limit": 3 });
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        let registry = Arc::new(ModuleRegistry::new(vec![module]).unwrap());
        SettingsStore::new(ConfigCompiler::new(registry).unwrap())
    }

    fn warn() -> ModuleId {
        ModuleId::new("warn").unwrap()
    }

    #[test]
    fn unconfigured_guilds_use_defaults() {
        let store = store();
        let settings = store.module(Id::new(1), &warn()).unwrap();
        assert!(!settings.enabled);
        assert_eq!(settings.config["limit"], 3);
        assert_eq!(store.configured_guilds(), 0);
    }

    #[test]
    fn yaml_edits_take_effect_for_that_guild_only() {
        let store = store();
        store
            .set_guild_yaml(
                Id::new(1),
                "modules:\n  warn:\n    enabled: true\n    config: { limit: 7 }\n",
            )
            .unwrap();
        let configured = store.module(Id::new(1), &warn()).unwrap();
        assert!(configured.enabled);
        assert_eq!(configured.config["limit"], 7);
        assert!(!store.module(Id::new(2), &warn()).unwrap().enabled);
    }

    #[test]
    fn an_invalid_edit_leaves_the_previous_settings_in_force() {
        let store = store();
        store
            .set_guild_yaml(Id::new(1), "modules:\n  warn:\n    enabled: true\n")
            .unwrap();
        let rejected =
            store.set_guild_yaml(Id::new(1), "modules:\n  warn:\n    config: { limit: 99 }\n");
        assert!(rejected.is_err());
        let rejected_syntax = store.set_guild_yaml(Id::new(1), "preset: [");
        assert!(rejected_syntax.is_err());
        assert!(store.module(Id::new(1), &warn()).unwrap().enabled);
    }

    #[test]
    fn language_prefers_the_override_then_discord_then_english() {
        let store = store();
        let guild = Id::new(1);
        assert_eq!(store.locale(guild), Locale::En);

        store.set_preferred_locale(guild, Some(Locale::Ja));
        assert_eq!(store.locale(guild), Locale::Ja);

        store.set_guild_yaml(guild, "language: en").unwrap();
        assert_eq!(store.locale(guild), Locale::En);

        store.set_guild_yaml(guild, "language: auto").unwrap();
        assert_eq!(store.locale(guild), Locale::Ja);

        store.set_preferred_locale(guild, None);
        assert_eq!(store.locale(guild), Locale::En);
    }

    #[test]
    fn the_guild_policy_follows_the_document() {
        let store = store();
        let guild = Id::new(1);
        assert_eq!(*store.policy(guild), GuildPolicy::default());

        store
            .set_guild_yaml(guild, "log_channel: 99\nmoderator_roles: [5, 6]\n")
            .unwrap();
        let policy = store.policy(guild);
        assert_eq!(policy.log_channel, Some(Id::new(99)));
        assert!(policy.is_moderator(&[Id::new(6)]));
        assert_eq!(
            *store.policy(Id::new(2)),
            GuildPolicy::default(),
            "other guilds are unaffected"
        );
    }

    #[test]
    fn the_highest_revision_wins_however_installs_are_ordered() {
        let store = store();
        let guild = Id::new(1);
        let compile = |yaml: &str| {
            store
                .compiler()
                .compile(&crate::parse_overrides(yaml).unwrap())
                .unwrap()
        };
        assert!(store.install(guild, 2, compile("language: ja")));
        assert!(
            !store.install(guild, 1, compile("language: en")),
            "stale install is refused"
        );
        assert_eq!(store.locale(guild), Locale::Ja);
        assert!(
            store.install(guild, 2, compile("language: en")),
            "same revision may refresh"
        );
        assert!(store.install(guild, 3, compile("language: ja")));
        assert_eq!(store.locale(guild), Locale::Ja);
    }

    #[test]
    fn runtime_limits_follow_the_preset_and_overrides() {
        let store = store();
        let guild = Id::new(1);
        let medium = store.limits(guild);
        assert_eq!(medium.concurrency, 8);
        assert_eq!(medium.handler_timeout, Duration::from_secs(10));

        store
            .set_guild_yaml(
                guild,
                "preset: large\nlimits:\n  handler_timeout_secs: 20\n",
            )
            .unwrap();
        let large = store.limits(guild);
        assert_eq!(large.concurrency, 32);
        assert_eq!(large.queue_capacity, 1024);
        assert_eq!(large.handler_timeout, Duration::from_secs(20));
    }

    #[test]
    fn removing_a_guild_restores_defaults() {
        let store = store();
        let guild = Id::new(1);
        store.set_guild_yaml(guild, "language: ja").unwrap();
        store.remove_guild(guild);
        assert_eq!(store.locale(guild), Locale::En);
        assert_eq!(store.configured_guilds(), 0);
    }
}
