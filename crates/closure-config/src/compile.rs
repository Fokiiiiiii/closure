use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use closure_core::capability::CapabilitySet;
use closure_core::module::{ModuleId, ModuleManifest, ModuleRuntime};
use closure_core::permission::{GuildPermissions, KeyRule, Level, PermissionKey};
use closure_core::registry::ModuleRegistry;
use closure_core::settings::{GuildPolicy, ModuleSettings};
use jsonschema::Validator;
use serde_json::Value;
use twilight_model::id::Id;

use crate::error::{ConfigError, Issue};
use crate::model::{
    CURRENT_VERSION, EffectiveConfig, GuildOverrides, GuildSettings, KeyOverride, LevelSetting,
    ModuleEffective, ModuleOverride, Principals,
};

const MAX_MODERATOR_ROLES: usize = 100;

/// How many entries each list of the `permissions` section may hold.
const MAX_PERMISSION_ENTRIES: usize = 200;

/// A document may not grow beyond this once parsed. The YAML text is capped much lower, but
/// aliases can expand a small text into a large value, and the result is stored twice (current
/// and history).
pub(crate) const MAX_DOCUMENT_BYTES: usize = 128 * 1024;

/// Turns sparse [`GuildOverrides`] into validated [`GuildSettings`].
///
/// Module config schemas are compiled once here, so a module that ships an unusable schema
/// stops startup instead of failing on the first guild that configures it.
pub struct ConfigCompiler {
    registry: Arc<ModuleRegistry>,
    validators: HashMap<ModuleId, Validator>,
    /// What every guild starts from, in the form of a document: the operator's choices for the
    /// whole bot. A guild's own document is merged over it.
    bot_defaults: Option<Value>,
    defaults: Arc<GuildSettings>,
}

impl std::fmt::Debug for ConfigCompiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigCompiler")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl ConfigCompiler {
    pub fn new(registry: Arc<ModuleRegistry>) -> Result<Self, ConfigError> {
        Self::with_publisher(registry, crate::schema::document_schema)
    }

    /// [`Self::new`] with the way the published document is made given, so that a document no
    /// validator can be built from can be put to the test.
    fn with_publisher(
        registry: Arc<ModuleRegistry>,
        publish: impl FnOnce(&ModuleRegistry) -> Result<serde_json::Value, ConfigError>,
    ) -> Result<Self, ConfigError> {
        let mut validators = HashMap::new();
        for module in registry.iter() {
            let manifest = module.manifest();
            // A schema that cannot be published faithfully is refused now, not published wrongly.
            crate::schema::check_supported(&manifest.config_schema).map_err(|message| {
                ConfigError::BadModuleSchema {
                    module: manifest.id.to_string(),
                    message,
                }
            })?;
            let validator =
                jsonschema::validator_for(&manifest.config_schema).map_err(|error| {
                    ConfigError::BadModuleSchema {
                        module: manifest.id.to_string(),
                        message: error.to_string(),
                    }
                })?;
            validators.insert(manifest.id.clone(), validator);
        }
        // The schemas must also make a document together that can be published, and one that a
        // validator can be built from.
        let published = publish(&registry)?;
        jsonschema::validator_for(&published).map_err(|error| {
            ConfigError::Serialize(format!("the published schema does not build: {error}"))
        })?;
        // The module defaults must satisfy their own schemas, otherwise no guild could ever
        // run on defaults.
        let defaults = Arc::new(compile_with(
            &registry,
            &validators,
            &GuildOverrides::default(),
            None,
        )?);
        Ok(Self {
            registry,
            validators,
            bot_defaults: None,
            defaults,
        })
    }

    /// Makes `defaults` the bottom layer of every guild's configuration: preset, then these
    /// bot-wide defaults, then the guild's own document. They must make a valid configuration
    /// on their own.
    pub fn with_bot_defaults(mut self, defaults: &GuildOverrides) -> Result<Self, ConfigError> {
        self.bot_defaults = None;
        let mut value = serde_json::to_value(defaults)
            .map_err(|error| ConfigError::Serialize(error.to_string()))?;
        compile_with(&self.registry, &self.validators, defaults, None)?;
        canonical_ids(&mut value);
        self.bot_defaults = Some(value);
        self.defaults = Arc::new(self.compile(&GuildOverrides::default())?);
        Ok(self)
    }

    #[must_use]
    pub fn registry(&self) -> &Arc<ModuleRegistry> {
        &self.registry
    }

    /// The settings of a guild that has configured nothing.
    #[must_use]
    pub fn defaults(&self) -> &Arc<GuildSettings> {
        &self.defaults
    }

    pub fn compile(&self, overrides: &GuildOverrides) -> Result<GuildSettings, ConfigError> {
        let Some(bot) = &self.bot_defaults else {
            return compile_with(&self.registry, &self.validators, overrides, None);
        };
        // What one document cannot be, it cannot be in the merge either: its own mistakes are
        // looked for before the merge makes them disappear.
        let mut issues = Vec::new();
        duplicate_ids(overrides, &mut issues);
        if !issues.is_empty() {
            return Err(ConfigError::Invalid(issues));
        }
        let mut own = serde_json::to_value(overrides)
            .map_err(|error| ConfigError::Serialize(error.to_string()))?;
        canonical_ids(&mut own);
        let layered: GuildOverrides = serde_json::from_value(merge(bot, &own))
            .map_err(|error| ConfigError::Serialize(error.to_string()))?;
        compile_with(
            &self.registry,
            &self.validators,
            &layered,
            Some(Layers {
                bot,
                own: overrides,
            }),
        )
    }
}

/// The documents a layered configuration was made of. A module's configuration is built from
/// them one layer after the other (the module's defaults, the bot's, the guild's), which merging
/// the two documents first would not give when a layer changes the shape of a value.
#[derive(Clone, Copy)]
struct Layers<'a> {
    bot: &'a Value,
    own: &'a GuildOverrides,
}

impl<'a> Layers<'a> {
    /// The configuration of `module` in each layer, bottom first.
    fn module_configs(self, module: &str) -> [Option<&'a Value>; 2] {
        [
            self.bot
                .get("modules")
                .and_then(|modules| modules.get(module))
                .and_then(|module| module.get("config")),
            self.own
                .modules
                .get(module)
                .and_then(|module| module.config.as_ref()),
        ]
    }
}

fn compile_with(
    registry: &ModuleRegistry,
    validators: &HashMap<ModuleId, Validator>,
    overrides: &GuildOverrides,
    layers: Option<Layers<'_>>,
) -> Result<GuildSettings, ConfigError> {
    if let Some(found) = overrides.version
        && found != CURRENT_VERSION
    {
        return Err(ConfigError::UnsupportedVersion {
            found,
            supported: CURRENT_VERSION,
        });
    }
    let size = serde_json::to_vec(overrides).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_DOCUMENT_BYTES {
        return Err(ConfigError::TooLarge {
            size,
            limit: MAX_DOCUMENT_BYTES,
        });
    }

    let mut issues = Vec::new();
    let preset = overrides.preset.unwrap_or_default();
    let mut limits = preset.limits();
    if let Some(limits_override) = &overrides.limits {
        limits_override.apply_to(&mut limits);
    }
    limits.validate(&mut issues);

    let moderator_roles = overrides.moderator_roles.clone().unwrap_or_default();
    if moderator_roles.len() > MAX_MODERATOR_ROLES {
        issues.push(Issue::new(
            "moderator_roles",
            "too_many",
            format!("at most {MAX_MODERATOR_ROLES} roles are allowed"),
        ));
    }

    let permissions = compile_permissions(registry, overrides, &mut issues);
    check_known_modules(registry, overrides, &mut issues);

    let mut effective_modules = BTreeMap::new();
    let mut module_settings = HashMap::new();
    for module in registry.iter() {
        let manifest = module.manifest();
        let module_override = overrides.modules.get(manifest.id.as_str());
        let configs = match layers {
            Some(layers) => layers.module_configs(manifest.id.as_str()),
            None => [module_override.and_then(|m| m.config.as_ref()), None],
        };
        let effective = compile_module(
            manifest,
            module_override,
            &configs,
            validators.get(&manifest.id),
            &mut issues,
        );
        module_settings.insert(
            manifest.id.clone(),
            ModuleSettings {
                enabled: effective.enabled,
                config: Arc::new(effective.config.clone()),
                granted: effective.capabilities.clone(),
            },
        );
        effective_modules.insert(manifest.id.to_string(), effective);
    }

    if !issues.is_empty() {
        return Err(ConfigError::Invalid(issues));
    }

    let policy = Arc::new(GuildPolicy {
        log_channel: overrides.log_channel,
        moderator_roles: moderator_roles.clone(),
    });
    Ok(GuildSettings {
        effective: EffectiveConfig {
            version: CURRENT_VERSION,
            preset,
            language: overrides.language.unwrap_or_default(),
            log_channel: overrides.log_channel,
            moderator_roles,
            limits,
            permissions: overrides.permissions.clone().unwrap_or_default(),
            modules: effective_modules,
        },
        modules: module_settings,
        policy,
        permissions: Arc::new(permissions),
    })
}

/// The guild's decisions about levels and permission keys, with what every module declared as
/// the starting point. `moderator_roles` is the older way to say "these roles are moderators".
fn compile_permissions(
    registry: &ModuleRegistry,
    overrides: &GuildOverrides,
    issues: &mut Vec<Issue>,
) -> GuildPermissions {
    let mut permissions = GuildPermissions::default();
    for spec in registry.permissions() {
        permissions
            .defaults
            .insert(spec.key.clone(), spec.default_level);
    }
    for role in overrides.moderator_roles.iter().flatten() {
        permissions.role_levels.insert(*role, Level::MODERATOR);
    }
    let Some(settings) = &overrides.permissions else {
        return permissions;
    };

    if let Some(setting) = &settings.administrator_level
        && let Some(level) = level_of(setting, "permissions.administrator_level", issues)
    {
        permissions.administrator_level = level;
    }
    duplicate_ids(overrides, issues);
    for (id, setting) in &settings.roles {
        let path = format!("permissions.roles.{id}");
        if let Some(role) = snowflake(id, &path, issues)
            && let Some(level) = level_of(setting, &path, issues)
        {
            permissions.role_levels.insert(role, level);
        }
    }
    for (id, setting) in &settings.users {
        let path = format!("permissions.users.{id}");
        if let Some(user) = snowflake(id, &path, issues)
            && let Some(level) = level_of(setting, &path, issues)
        {
            permissions.user_levels.insert(user, level);
        }
    }
    for (name, key_override) in &settings.keys {
        let path = format!("permissions.keys.{name}");
        let Some(key) = known_key(registry, name, &path, issues) else {
            continue;
        };
        let rule = compile_key_rule(key_override, &path, issues);
        permissions.keys.insert(key, rule);
    }
    for (path, len) in [
        ("permissions.roles", settings.roles.len()),
        ("permissions.users", settings.users.len()),
        ("permissions.keys", settings.keys.len()),
    ] {
        too_many(path, len, issues);
    }
    permissions
}

/// Ids written as map keys that are the same id in two ways (`5` and `05`).
fn duplicate_ids(overrides: &GuildOverrides, issues: &mut Vec<Issue>) {
    let Some(settings) = &overrides.permissions else {
        return;
    };
    for (section, ids) in [
        ("roles", settings.roles.keys().collect::<Vec<_>>()),
        ("users", settings.users.keys().collect::<Vec<_>>()),
    ] {
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(canonical_id(id)) {
                issues.push(Issue::new(
                    format!("permissions.{section}.{id}"),
                    "duplicate_id",
                    "this id is written twice (`5` and `05` are the same id)",
                ));
            }
        }
    }
}

/// The way an id is written when it is a number, so the same id written two ways is one key.
fn canonical_id(id: &str) -> String {
    id.parse::<u64>()
        .map_or_else(|_| id.to_owned(), |number| number.to_string())
}

/// Rewrites the ids of the permission maps of a document in their canonical form.
fn canonical_ids(document: &mut Value) {
    for section in ["roles", "users"] {
        let Some(Value::Object(map)) = document
            .get_mut("permissions")
            .and_then(|permissions| permissions.get_mut(section))
        else {
            continue;
        };
        let rewritten: serde_json::Map<String, Value> = std::mem::take(map)
            .into_iter()
            .map(|(id, level)| (canonical_id(&id), level))
            .collect();
        *map = rewritten;
    }
}

fn too_many(path: &str, len: usize, issues: &mut Vec<Issue>) {
    if len > MAX_PERMISSION_ENTRIES {
        issues.push(Issue::new(
            path,
            "too_many",
            format!("at most {MAX_PERMISSION_ENTRIES} entries are allowed"),
        ));
    }
}

fn level_of(setting: &LevelSetting, path: &str, issues: &mut Vec<Issue>) -> Option<Level> {
    setting
        .resolve()
        .map_err(|message| issues.push(Issue::new(path, "invalid_level", message)))
        .ok()
}

/// An id written as a map key.
fn snowflake<M>(text: &str, path: &str, issues: &mut Vec<Issue>) -> Option<Id<M>> {
    let id = text.parse::<u64>().ok().and_then(Id::new_checked);
    if id.is_none() {
        issues.push(Issue::new(
            path,
            "invalid_id",
            "expected a Discord id: a positive number",
        ));
    }
    id
}

fn known_key(
    registry: &ModuleRegistry,
    name: &str,
    path: &str,
    issues: &mut Vec<Issue>,
) -> Option<PermissionKey> {
    let key = match PermissionKey::new(name) {
        Ok(key) => key,
        Err(error) => {
            issues.push(Issue::new(
                path,
                "invalid_permission_key",
                error.to_string(),
            ));
            return None;
        }
    };
    if registry.permission(&key).is_none() {
        let available: Vec<&str> = registry
            .permissions()
            .map(|spec| spec.key.as_str())
            .collect();
        issues.push(Issue::new(
            path,
            "unknown_permission",
            format!("no module declares it; available: {}", available.join(", ")),
        ));
        return None;
    }
    Some(key)
}

fn compile_key_rule(rule: &KeyOverride, path: &str, issues: &mut Vec<Issue>) -> KeyRule {
    let min_level = rule
        .min_level
        .as_ref()
        .and_then(|setting| level_of(setting, &format!("{path}.min_level"), issues));
    let mut principals = |side: &str, principals: Option<&Principals>| {
        let roles = principals.and_then(|p| p.roles.clone()).unwrap_or_default();
        let users = principals.and_then(|p| p.users.clone()).unwrap_or_default();
        too_many(&format!("{path}.{side}.roles"), roles.len(), issues);
        too_many(&format!("{path}.{side}.users"), users.len(), issues);
        (roles, users)
    };
    let (allow_roles, allow_users) = principals("allow", rule.allow.as_ref());
    let (deny_roles, deny_users) = principals("deny", rule.deny.as_ref());
    KeyRule {
        min_level,
        allow_roles,
        allow_users,
        deny_roles,
        deny_users,
    }
}

/// Rejects configuration for modules that do not exist; a typo must not silently do nothing.
fn check_known_modules(
    registry: &ModuleRegistry,
    overrides: &GuildOverrides,
    issues: &mut Vec<Issue>,
) {
    for key in overrides.modules.keys() {
        let known = ModuleId::new(key.as_str()).is_ok_and(|id| registry.get(&id).is_some());
        if !known {
            let available: Vec<String> = registry
                .iter()
                .map(|module| module.manifest().id.to_string())
                .collect();
            issues.push(Issue::new(
                format!("modules.{key}"),
                "unknown_module",
                format!("no such module; available: {}", available.join(", ")),
            ));
        }
    }
}

/// Combines a module's manifest defaults with a guild's override, recording every problem.
fn compile_module(
    manifest: &ModuleManifest,
    module_override: Option<&ModuleOverride>,
    configs: &[Option<&Value>; 2],
    validator: Option<&Validator>,
    issues: &mut Vec<Issue>,
) -> ModuleEffective {
    let id = manifest.id.as_str();
    let enabled = module_override
        .and_then(|m| m.enabled)
        .unwrap_or(manifest.default_enabled);

    // Native modules get what they declare; sandboxed runtimes start with nothing and need an
    // explicit grant.
    let capabilities = match module_override.and_then(|m| m.capabilities.as_ref()) {
        Some(requested) => {
            for capability in requested.missing_from(&manifest.capabilities) {
                issues.push(Issue::new(
                    format!("modules.{id}.capabilities"),
                    "capability_not_declared",
                    format!("module `{id}` does not declare capability `{capability}`"),
                ));
            }
            requested.intersection(&manifest.capabilities)
        }
        None if manifest.runtime == ModuleRuntime::Native => manifest.capabilities.clone(),
        None => CapabilitySet::new(),
    };

    let config = configs
        .iter()
        .flatten()
        .fold(manifest.default_config.clone(), |config, layer| {
            merge(&config, layer)
        });
    // The documents as they were written too: a null-valued key is dropped by the merge, yet is
    // still stored.
    let raw_has_nul = configs.iter().flatten().any(|layer| contains_nul(layer));
    if raw_has_nul || contains_nul(&config) {
        // PostgreSQL cannot store U+0000 in JSON text; refuse it here as a clear 422.
        issues.push(Issue::new(
            format!("modules.{id}.config"),
            "invalid_module_config",
            "text must not contain the NUL character (U+0000)",
        ));
    }
    if let Some(validator) = validator {
        for error in validator.iter_errors(&config) {
            issues.push(Issue::new(
                format!(
                    "modules.{id}.config{}",
                    pointer_to_path(&error.instance_path().to_string())
                ),
                "invalid_module_config",
                error.to_string(),
            ));
        }
    }

    ModuleEffective {
        enabled,
        capabilities,
        config,
    }
}

fn contains_nul(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains('\0'),
        Value::Array(items) => items.iter().any(contains_nul),
        Value::Object(map) => map
            .iter()
            .any(|(key, item)| key.contains('\0') || contains_nul(item)),
        _ => false,
    }
}

/// `/limit/0` to `.limit.0`, so paths read like the YAML.
fn pointer_to_path(pointer: &str) -> String {
    pointer
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(String::new(), |mut path, part| {
            path.push('.');
            path.push_str(part);
            path
        })
}

/// Deep-merges `over` onto `base`.
///
/// Objects merge key by key, everything else (including arrays) is replaced. A `null` in `over`
/// means "use the default": the key is left as in `base` and never written when `base` lacks it.
pub(crate) fn merge(base: &Value, over: &Value) -> Value {
    match (base, over) {
        (_, Value::Null) => base.clone(),
        (Value::Object(base_map), Value::Object(over_map)) => {
            let mut merged = base_map.clone();
            for (key, over_value) in over_map {
                if over_value.is_null() {
                    continue;
                }
                let value = match base_map.get(key) {
                    Some(base_value) => merge(base_value, over_value),
                    None => strip_nulls(over_value),
                };
                merged.insert(key.clone(), value);
            }
            Value::Object(merged)
        }
        (_, other) => strip_nulls(other),
    }
}

fn strip_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), strip_nulls(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use closure_core::capability::Capability;
    use closure_core::module::{Module, ModuleManifest};
    use closure_core::permission::{Actor, PermissionSpec, Reason};
    use closure_core::testing::StubModule;
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::model::{LimitsOverride, ModuleOverride, PermissionsOverride, Preset};

    fn module(id: &str, build: impl FnOnce(ModuleManifest) -> ModuleManifest) -> Arc<dyn Module> {
        let manifest = ModuleManifest::new(ModuleId::new(id).unwrap(), id, "0.1.0");
        Arc::new(StubModule(build(manifest)))
    }

    #[test]
    fn a_published_document_no_validator_can_be_built_from_stops_startup() {
        let registry = Arc::new(ModuleRegistry::new(vec![module("warn", |m| m)]).unwrap());
        let Err(error) =
            ConfigCompiler::with_publisher(Arc::clone(&registry), |_| Ok(json!({ "type": 5 })))
        else {
            panic!("the bot must not start");
        };
        assert!(
            error
                .to_string()
                .contains("the published schema does not build"),
            "{error}"
        );
        ConfigCompiler::with_publisher(registry, |_| Ok(json!({ "type": "object" }))).unwrap();
    }

    fn warn_module() -> Arc<dyn Module> {
        module("warn", |mut manifest| {
            manifest.config_schema = json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 10 },
                    "notify": { "type": "boolean" }
                },
                "additionalProperties": false
            });
            manifest.default_config = json!({ "limit": 3, "notify": true });
            manifest.capabilities = [Capability::SendMessages, Capability::TimeoutMembers]
                .into_iter()
                .collect();
            manifest.default_enabled = true;
            manifest.permissions = vec![
                PermissionSpec::new("warn.add", Level::MODERATOR, "give a warning").unwrap(),
                PermissionSpec::new("warn.clear", Level::ADMIN, "remove warnings").unwrap(),
            ];
            manifest
        })
    }

    fn compiler() -> ConfigCompiler {
        let registry = ModuleRegistry::new(vec![warn_module(), module("tools", |m| m)]).unwrap();
        ConfigCompiler::new(Arc::new(registry)).unwrap()
    }

    fn warn_override(config: Value) -> GuildOverrides {
        let mut overrides = GuildOverrides::default();
        overrides.modules.insert(
            "warn".into(),
            ModuleOverride {
                config: Some(config),
                ..ModuleOverride::default()
            },
        );
        overrides
    }

    fn compiled(yaml: &str) -> Result<GuildSettings, ConfigError> {
        compiler().compile(&crate::yaml::parse_overrides(yaml).unwrap())
    }

    fn issues_of(yaml: &str) -> Vec<(String, &'static str)> {
        let Err(ConfigError::Invalid(issues)) = compiled(yaml) else {
            panic!("the document must be refused: {yaml}");
        };
        issues
            .into_iter()
            .map(|issue| (issue.path, issue.code))
            .collect()
    }

    fn actor(user: u64, roles: &[u64]) -> Actor {
        Actor {
            user: Id::new(user),
            roles: roles.iter().map(|id| Id::new(*id)).collect(),
            administrator: false,
            owner: false,
        }
    }

    fn key(text: &str) -> PermissionKey {
        PermissionKey::new(text).unwrap()
    }

    #[test]
    fn what_modules_declare_is_the_default_of_every_guild() {
        let settings = compiler().compile(&GuildOverrides::default()).unwrap();
        let permissions = &settings.permissions;
        assert_eq!(permissions.defaults.len(), 2);
        assert!(!permissions.check(&actor(1, &[]), &key("warn.add")).allowed);
        let mut admin = actor(1, &[]);
        admin.administrator = true;
        assert!(permissions.check(&admin, &key("warn.add")).allowed);
        assert!(permissions.check(&admin, &key("warn.clear")).allowed);
        assert_eq!(
            settings.effective.permissions,
            PermissionsOverride::default()
        );
    }

    #[test]
    fn the_permissions_section_sets_levels_and_decides_single_keys() {
        let settings = compiled(
            "
moderator_roles: [5]
permissions:
  administrator_level: moderator
  roles: { 10: trusted, 11: 60 }
  users: { 20: admin }
  keys:
    warn.add:
      min_level: trusted
      allow: { users: [30] }
      deny: { roles: [12] }
",
        )
        .unwrap();
        let permissions = &settings.permissions;
        assert_eq!(permissions.administrator_level, Level::MODERATOR);
        // The older `moderator_roles` still makes moderators.
        assert!(permissions.check(&actor(1, &[5]), &key("warn.add")).allowed);
        assert!(
            permissions
                .check(&actor(1, &[10]), &key("warn.add"))
                .allowed
        );
        assert!(!permissions.check(&actor(1, &[]), &key("warn.add")).allowed);
        // Levels from roles and from a member's own entry; `warn.clear` stays at the default.
        assert_eq!(
            permissions.level_of(&actor(1, &[11])),
            Level::new(60).unwrap()
        );
        assert!(
            permissions
                .check(&actor(20, &[]), &key("warn.clear"))
                .allowed
        );
        assert!(
            !permissions
                .check(&actor(1, &[11]), &key("warn.clear"))
                .allowed
        );
        // An allowance beats the level, a refusal beats both.
        assert_eq!(
            permissions.check(&actor(30, &[]), &key("warn.add")).reason,
            Reason::AllowedUser
        );
        assert_eq!(
            permissions
                .check(&actor(20, &[12]), &key("warn.add"))
                .reason,
            Reason::DeniedRole(Id::new(12))
        );
        // What was written is shown back.
        assert!(settings.effective.permissions.keys.contains_key("warn.add"));
    }

    #[test]
    fn a_role_in_the_permissions_section_beats_moderator_roles() {
        let settings =
            compiled("moderator_roles: [5]\npermissions:\n  roles: { 5: general }\n").unwrap();
        assert!(
            !settings
                .permissions
                .check(&actor(1, &[5]), &key("warn.add"))
                .allowed
        );
    }

    #[test]
    fn mistakes_in_the_permissions_section_are_reported_with_their_place() {
        let issues = issues_of(
            "
permissions:
  administrator_level: 101
  roles: { abc: moderator, 0: moderator, 7: boss, 8: 0 }
  users: { 9: nobody }
  keys:
    warn.unheard: { min_level: admin }
    Not-A-Key: {}
    warn.add: { min_level: 200 }
",
        );
        for expected in [
            ("permissions.administrator_level", "invalid_level"),
            ("permissions.roles.abc", "invalid_id"),
            ("permissions.roles.0", "invalid_id"),
            ("permissions.roles.7", "invalid_level"),
            ("permissions.roles.8", "invalid_level"),
            ("permissions.users.9", "invalid_level"),
            ("permissions.keys.warn.unheard", "unknown_permission"),
            ("permissions.keys.Not-A-Key", "invalid_permission_key"),
            ("permissions.keys.warn.add.min_level", "invalid_level"),
        ] {
            assert!(
                issues
                    .iter()
                    .any(|(path, code)| (path.as_str(), *code) == expected),
                "{expected:?} missing from {issues:?}"
            );
        }
    }

    #[test]
    fn the_lists_of_the_permissions_section_are_bounded() {
        let many: Vec<String> = (1..=201).map(|id| format!("{id}: trusted")).collect();
        let issues = issues_of(&format!(
            "permissions:\n  roles: {{ {} }}\n",
            many.join(", ")
        ));
        assert_eq!(issues, [("permissions.roles".to_owned(), "too_many")]);
        let roles: Vec<String> = (1..=201).map(|id| id.to_string()).collect();
        let issues = issues_of(&format!(
            "permissions:\n  keys:\n    warn.add:\n      deny: {{ roles: [{}] }}\n",
            roles.join(", ")
        ));
        assert_eq!(
            issues,
            [(
                "permissions.keys.warn.add.deny.roles".to_owned(),
                "too_many"
            )]
        );
    }

    #[test]
    fn bot_wide_defaults_sit_between_the_preset_and_the_guild() {
        let bot = crate::yaml::parse_overrides(
            "
language: ja
moderator_roles: [5]
limits: { concurrency: 12 }
modules:
  warn:
    config: { limit: 6, notify: false }
",
        )
        .unwrap();
        let compiler = compiler().with_bot_defaults(&bot).unwrap();
        let warn = ModuleId::new("warn").unwrap();

        // A guild that configured nothing gets them.
        let defaults = compiler.defaults();
        assert_eq!(
            defaults.effective.language.0,
            Some(closure_core::locale::Locale::Ja)
        );
        assert_eq!(defaults.effective.limits.concurrency, 12);
        assert_eq!(
            *defaults.module(&warn).unwrap().config,
            json!({ "limit": 6, "notify": false })
        );

        // A guild's own document wins, key by key; what it leaves out comes from the bot.
        let own = crate::yaml::parse_overrides(
            "
language: en
modules:
  warn:
    config: { limit: 8, notify: null }
",
        )
        .unwrap();
        let settings = compiler.compile(&own).unwrap();
        assert_eq!(
            settings.effective.language.0,
            Some(closure_core::locale::Locale::En)
        );
        assert_eq!(settings.effective.limits.concurrency, 12);
        assert_eq!(
            *settings.module(&warn).unwrap().config,
            json!({ "limit": 8, "notify": false })
        );
        assert!(
            settings
                .permissions
                .check(&actor(1, &[5]), &key("warn.add"))
                .allowed
        );
    }

    #[test]
    fn bot_wide_defaults_that_are_not_a_valid_configuration_stop_startup() {
        for yaml in [
            "modules: { warn: { config: { limit: 99 } } }",
            "modules: { nope: {} }",
            "permissions: { keys: { warn.unheard: {} } }",
        ] {
            let bot = crate::yaml::parse_overrides(yaml).unwrap();
            assert!(compiler().with_bot_defaults(&bot).is_err(), "{yaml}");
        }
    }

    #[test]
    fn empty_overrides_give_manifest_defaults() {
        let settings = compiler().compile(&GuildOverrides::default()).unwrap();
        let warn = settings.module(&ModuleId::new("warn").unwrap()).unwrap();
        assert!(warn.enabled);
        assert_eq!(*warn.config, json!({ "limit": 3, "notify": true }));
        assert!(warn.granted.contains(Capability::TimeoutMembers));
        let tools = settings.module(&ModuleId::new("tools").unwrap()).unwrap();
        assert!(!tools.enabled);
        assert_eq!(settings.effective.preset, Preset::Medium);
        assert_eq!(settings.effective.limits, Preset::Medium.limits());
    }

    #[test]
    fn preset_changes_defaults_and_limits_override_individual_values() {
        let overrides = GuildOverrides {
            preset: Some(Preset::Large),
            limits: Some(LimitsOverride {
                concurrency: Some(5),
                ..LimitsOverride::default()
            }),
            ..GuildOverrides::default()
        };
        let limits = compiler().compile(&overrides).unwrap().effective.limits;
        assert_eq!(limits.queue_capacity, Preset::Large.limits().queue_capacity);
        assert_eq!(limits.concurrency, 5);
    }

    #[test]
    fn module_config_is_merged_onto_its_defaults() {
        let settings = compiler()
            .compile(&warn_override(json!({ "limit": 5 })))
            .unwrap();
        let warn = settings.module(&ModuleId::new("warn").unwrap()).unwrap();
        assert_eq!(*warn.config, json!({ "limit": 5, "notify": true }));
    }

    #[test]
    fn module_config_violating_its_schema_is_reported_with_a_path() {
        let error = compiler()
            .compile(&warn_override(json!({ "limit": 99 })))
            .unwrap_err();
        let issue = &error.issues()[0];
        assert_eq!(issue.code, "invalid_module_config");
        assert_eq!(issue.path, "modules.warn.config.limit");
    }

    #[test]
    fn unknown_module_is_rejected_to_catch_typos() {
        let mut overrides = GuildOverrides::default();
        overrides
            .modules
            .insert("wran".into(), ModuleOverride::default());
        let error = compiler().compile(&overrides).unwrap_err();
        assert_eq!(error.issues()[0].code, "unknown_module");
        assert!(error.issues()[0].message.contains("warn"));
    }

    #[test]
    fn capabilities_can_narrow_but_never_exceed_the_declaration() {
        let mut overrides = GuildOverrides::default();
        overrides.modules.insert(
            "warn".into(),
            ModuleOverride {
                capabilities: Some([Capability::SendMessages].into_iter().collect()),
                ..ModuleOverride::default()
            },
        );
        let settings = compiler().compile(&overrides).unwrap();
        let granted = &settings
            .module(&ModuleId::new("warn").unwrap())
            .unwrap()
            .granted;
        assert!(granted.contains(Capability::SendMessages));
        assert!(!granted.contains(Capability::TimeoutMembers));

        overrides.modules.insert(
            "warn".into(),
            ModuleOverride {
                capabilities: Some([Capability::BanMembers].into_iter().collect()),
                ..ModuleOverride::default()
            },
        );
        let error = compiler().compile(&overrides).unwrap_err();
        assert_eq!(error.issues()[0].code, "capability_not_declared");
    }

    #[test]
    fn sandboxed_runtimes_start_with_no_capabilities() {
        let wasm = module("script", |mut manifest| {
            manifest.runtime = ModuleRuntime::Wasm;
            manifest.capabilities = [Capability::BanMembers].into_iter().collect();
            manifest
        });
        let compiler =
            ConfigCompiler::new(Arc::new(ModuleRegistry::new(vec![wasm]).unwrap())).unwrap();
        let settings = compiler.compile(&GuildOverrides::default()).unwrap();
        let script = settings.module(&ModuleId::new("script").unwrap()).unwrap();
        assert!(script.granted.is_empty());
    }

    #[test]
    fn out_of_range_limits_are_all_reported_at_once() {
        let overrides = GuildOverrides {
            limits: Some(LimitsOverride {
                handler_timeout_secs: Some(0),
                queue_capacity: Some(1),
                concurrency: Some(1000),
            }),
            ..GuildOverrides::default()
        };
        let error = compiler().compile(&overrides).unwrap_err();
        assert_eq!(error.issues().len(), 3);
    }

    #[test]
    fn newer_schema_versions_are_refused() {
        let overrides = GuildOverrides {
            version: Some(CURRENT_VERSION + 1),
            ..GuildOverrides::default()
        };
        assert!(matches!(
            compiler().compile(&overrides),
            Err(ConfigError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn a_module_whose_defaults_break_its_schema_stops_startup() {
        let broken = module("broken", |mut manifest| {
            manifest.config_schema = json!({ "type": "object", "required": ["token"] });
            manifest
        });
        let registry = Arc::new(ModuleRegistry::new(vec![broken]).unwrap());
        assert!(matches!(
            ConfigCompiler::new(registry),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn an_unusable_schema_stops_startup() {
        let broken = module("broken", |mut manifest| {
            manifest.config_schema = json!({ "type": 12 });
            manifest
        });
        let registry = Arc::new(ModuleRegistry::new(vec![broken]).unwrap());
        assert!(matches!(
            ConfigCompiler::new(registry),
            Err(ConfigError::BadModuleSchema { .. })
        ));
    }

    #[test]
    fn documents_that_grow_too_large_once_parsed_are_refused() {
        let huge = "x".repeat(MAX_DOCUMENT_BYTES);
        let error = compiler()
            .compile(&warn_override(json!({ "note": huge })))
            .unwrap_err();
        assert!(matches!(error, ConfigError::TooLarge { .. }), "{error:?}");
    }

    #[test]
    fn text_containing_nul_is_a_clear_validation_error() {
        let error = compiler()
            .compile(&warn_override(json!({ "limit": 3, "note": "a\u{0}b" })))
            .unwrap_err();
        assert!(
            error
                .issues()
                .iter()
                .any(|i| i.code == "invalid_module_config" && i.message.contains("NUL")),
            "{error:?}"
        );
        let in_a_key = compiler()
            .compile(&warn_override(json!({ "nested": { "a\u{0}": 1 } })))
            .unwrap_err();
        assert!(in_a_key.issues().iter().any(|i| i.message.contains("NUL")));
    }

    #[test]
    fn nul_in_a_key_that_the_merge_drops_is_still_refused() {
        // `null` means "use the default", so the key vanishes from the merged value, but the
        // document PostgreSQL would be asked to store still contains it.
        let error = compiler()
            .compile(&warn_override(json!({ "bad\u{0}key": null })))
            .unwrap_err();
        assert!(
            error.issues().iter().any(|i| i.message.contains("NUL")),
            "{error:?}"
        );
    }

    #[test]
    fn null_resets_to_the_default_and_arrays_replace() {
        let base = json!({ "a": 1, "list": [1, 2], "nested": { "x": 1, "y": 2 } });
        let over = json!({ "a": null, "list": [9], "nested": { "x": 5, "y": null }, "new": { "k": null, "v": 1 } });
        assert_eq!(
            merge(&base, &over),
            json!({ "a": 1, "list": [9], "nested": { "x": 5, "y": 2 }, "new": { "v": 1 } })
        );
    }

    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i32>().prop_map(|n| json!(n)),
            "[a-z]{0,4}".prop_map(Value::String),
        ];
        leaf.prop_recursive(3, 24, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::btree_map("[a-c]", inner, 0..4)
                    .prop_map(|map| Value::Object(map.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #[test]
        fn merging_an_empty_object_into_an_object_changes_nothing(base in arb_json()) {
            prop_assume!(base.is_object());
            prop_assert_eq!(merge(&base, &json!({})), base);
        }

        #[test]
        fn merging_twice_equals_merging_once(base in arb_json(), over in arb_json()) {
            let once = merge(&base, &over);
            prop_assert_eq!(merge(&once, &over), once);
        }
    }
    fn cfg_module() -> Arc<dyn Module> {
        module("cfg", |mut manifest| {
            manifest.config_schema = json!({
                "type": "object",
                "properties": { "x": { "type": ["integer", "object"] } },
                "additionalProperties": false
            });
            manifest.default_config = json!({ "x": { "a": 1, "b": 2 } });
            manifest.default_enabled = true;
            manifest
        })
    }

    fn with_bot(bot: &str) -> ConfigCompiler {
        let registry =
            ModuleRegistry::new(vec![warn_module(), cfg_module(), module("tools", |m| m)]).unwrap();
        ConfigCompiler::new(Arc::new(registry))
            .unwrap()
            .with_bot_defaults(&crate::yaml::parse_overrides(bot).unwrap())
            .unwrap()
    }

    #[test]
    fn the_layers_apply_one_after_the_other_not_the_top_two_first() {
        // defaults {x:{a:1,b:2}}, then the bot's x: 0 replaces it, then the guild's {a: 3} replaces
        // that: nothing of the default's `b` is left to come back.
        let compiler = with_bot("modules: { cfg: { config: { x: 0 } } }");
        let own =
            crate::yaml::parse_overrides("modules: { cfg: { config: { x: { a: 3 } } } }").unwrap();
        let settings = compiler.compile(&own).unwrap();
        let cfg = ModuleId::new("cfg").unwrap();
        assert_eq!(
            *settings.module(&cfg).unwrap().config,
            json!({ "x": { "a": 3 } })
        );
        // What the guild leaves alone is the bot's.
        let nothing = compiler.compile(&GuildOverrides::default()).unwrap();
        assert_eq!(*nothing.module(&cfg).unwrap().config, json!({ "x": 0 }));
    }

    #[test]
    fn the_same_discord_id_written_two_ways_is_one_id_in_every_layer() {
        let compiler = with_bot("permissions: { users: { '5': admin }, roles: { '8': admin } }");
        let own = crate::yaml::parse_overrides(
            "permissions: { users: { '05': general }, roles: { '008': general } }",
        )
        .unwrap();
        let settings = compiler.compile(&own).unwrap();
        let level = settings.permissions.level_of(&actor(5, &[8]));
        assert_eq!(level, Level::GENERAL, "the guild lowers what the bot gave");
    }

    #[test]
    fn two_ways_of_writing_one_id_in_one_document_are_refused() {
        let issues = issues_of("permissions: { users: { '5': admin, '05': general } }");
        assert!(
            issues.iter().any(|(_, code)| *code == "duplicate_id"),
            "{issues:?}"
        );
    }

    #[test]
    fn a_nul_key_the_merge_would_drop_is_still_refused_with_bot_wide_defaults() {
        let own = warn_override(json!({ "bad\u{0}key": null }));
        let Err(ConfigError::Invalid(issues)) = compiler().compile(&own) else {
            panic!("without bot defaults it is refused");
        };
        assert!(issues.iter().any(|issue| issue.message.contains("NUL")));
        let Err(ConfigError::Invalid(issues)) = with_bot("{}").compile(&own) else {
            panic!("and with them too");
        };
        assert!(issues.iter().any(|issue| issue.message.contains("NUL")));
    }
}
