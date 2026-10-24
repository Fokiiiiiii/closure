use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use closure_core::capability::CapabilitySet;
use closure_core::module::{ModuleId, ModuleManifest, ModuleRuntime};
use closure_core::registry::ModuleRegistry;
use closure_core::settings::{GuildPolicy, ModuleSettings};
use jsonschema::Validator;
use serde_json::Value;

use crate::error::{ConfigError, Issue};
use crate::model::{
    CURRENT_VERSION, EffectiveConfig, GuildOverrides, GuildSettings, ModuleEffective,
    ModuleOverride,
};

const MAX_MODERATOR_ROLES: usize = 100;

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
        let mut validators = HashMap::new();
        for module in registry.iter() {
            let manifest = module.manifest();
            let validator =
                jsonschema::validator_for(&manifest.config_schema).map_err(|error| {
                    ConfigError::BadModuleSchema {
                        module: manifest.id.to_string(),
                        message: error.to_string(),
                    }
                })?;
            validators.insert(manifest.id.clone(), validator);
        }
        // The module defaults must satisfy their own schemas, otherwise no guild could ever
        // run on defaults.
        let defaults = Arc::new(compile_with(
            &registry,
            &validators,
            &GuildOverrides::default(),
        )?);
        Ok(Self {
            registry,
            validators,
            defaults,
        })
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
        compile_with(&self.registry, &self.validators, overrides)
    }
}

fn compile_with(
    registry: &ModuleRegistry,
    validators: &HashMap<ModuleId, Validator>,
    overrides: &GuildOverrides,
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

    check_known_modules(registry, overrides, &mut issues);

    let mut effective_modules = BTreeMap::new();
    let mut module_settings = HashMap::new();
    for module in registry.iter() {
        let manifest = module.manifest();
        let effective = compile_module(
            manifest,
            overrides.modules.get(manifest.id.as_str()),
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
            modules: effective_modules,
        },
        modules: module_settings,
        policy,
    })
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

    let config = merge_config(
        &manifest.default_config,
        module_override.and_then(|m| m.config.as_ref()),
    );
    // The raw override too: a null-valued key is dropped by the merge, yet is still stored.
    let raw_has_nul = module_override
        .and_then(|m| m.config.as_ref())
        .is_some_and(contains_nul);
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

fn merge_config(defaults: &Value, over: Option<&Value>) -> Value {
    over.map_or_else(|| defaults.clone(), |over| merge(defaults, over))
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
    use closure_core::testing::StubModule;
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::model::{LimitsOverride, ModuleOverride, Preset};

    fn module(id: &str, build: impl FnOnce(ModuleManifest) -> ModuleManifest) -> Arc<dyn Module> {
        let manifest = ModuleManifest::new(ModuleId::new(id).unwrap(), id, "0.1.0");
        Arc::new(StubModule(build(manifest)))
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
}
