use closure_core::capability::Capability;
use closure_core::locale::Locale;
use closure_core::registry::ModuleRegistry;
use serde_json::{Map, Value, json};

use crate::error::ConfigError;
use crate::model::GuildOverrides;

/// The JSON Schema of the whole configuration document, with every registered module's own
/// config schema embedded. Dashboards generate their forms from it and editors use it for
/// completion; it is derived from the same types the parser uses, so it cannot drift.
pub fn document_schema(registry: &ModuleRegistry) -> Result<Value, ConfigError> {
    let mut schema = serde_json::to_value(schemars::schema_for!(GuildOverrides))
        .map_err(|error| ConfigError::Serialize(error.to_string()))?;

    let mut languages = vec![json!("auto")];
    languages.extend(Locale::ALL.iter().map(|locale| json!(locale.tag())));

    let mut module_properties = Map::new();
    for module in registry.iter() {
        let manifest = module.manifest();
        let mut config_schema = manifest.config_schema.clone();
        if let Value::Object(map) = &mut config_schema {
            map.remove("$schema");
        }
        allow_null_properties(&mut config_schema);
        let capabilities: Vec<&str> = manifest
            .capabilities
            .iter()
            .map(Capability::as_str)
            .collect();
        module_properties.insert(
            manifest.id.to_string(),
            json!({
                "type": "object",
                "description": manifest.description,
                "additionalProperties": false,
                "properties": {
                    "enabled": { "type": "boolean", "default": manifest.default_enabled },
                    "capabilities": {
                        "type": "array",
                        "uniqueItems": true,
                        "items": { "enum": capabilities }
                    },
                    "config": config_schema
                }
            }),
        );
    }

    if let Some(Value::Object(properties)) = schema.pointer_mut("/properties") {
        properties.insert(
            "language".into(),
            json!({
                "description": "Language of the bot's messages; `auto` follows the server's community locale.",
                "type": "string",
                "enum": languages,
                "default": "auto"
            }),
        );
        properties.insert(
            "modules".into(),
            json!({
                "description": "Per-module settings, keyed by module id.",
                "type": "object",
                "additionalProperties": false,
                "properties": module_properties
            }),
        );
    }
    Ok(schema)
}

/// In a module's configuration `null` means "use the default", so the published schema must
/// accept it wherever a property may be set. (Array items are replaced whole, so `null` has no
/// special meaning there and they stay strict.)
///
/// Properties can sit in the schema itself or in a keyword that only *adds* constraints to the
/// same object: `allOf`, `anyOf`, `then`, `else` and `patternProperties`.
///
/// Keywords that *decide* something are left as written, because relaxing them changes the
/// decision: `if` picks `then` or `else`, and `oneOf` demands exactly one matching branch. The
/// compiler judges the document after `null` has been replaced by the default, which the schema
/// cannot express there, so a `null` under such a keyword is judged on its own.
fn allow_null_properties(schema: &mut Value) {
    let Value::Object(map) = schema else { return };
    let mut nullable = |sub: &mut Value| {
        allow_null_properties(sub);
        let inner = sub.take();
        *sub = json!({ "anyOf": [inner, { "type": "null" }] });
    };
    for keyword in ["properties", "patternProperties"] {
        if let Some(Value::Object(properties)) = map.get_mut(keyword) {
            properties.values_mut().for_each(&mut nullable);
        }
    }
    if let Some(additional @ Value::Object(_)) = map.get_mut("additionalProperties") {
        nullable(additional);
    }
    for keyword in ["allOf", "anyOf"] {
        if let Some(Value::Array(branches)) = map.get_mut(keyword) {
            branches.iter_mut().for_each(allow_null_properties);
        }
    }
    for keyword in ["then", "else"] {
        if let Some(branch) = map.get_mut(keyword) {
            allow_null_properties(branch);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::capability::Capability;
    use closure_core::module::{Module, ModuleId, ModuleManifest};
    use closure_core::testing::StubModule;

    use super::*;
    use crate::model::{ModuleOverride, Preset};
    use crate::yaml::overrides_to_yaml;

    fn registry() -> ModuleRegistry {
        registry_with(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "limit": { "type": "integer" } }
        }))
    }

    fn registry_with(config_schema: Value) -> ModuleRegistry {
        registry_with_defaults(config_schema, json!({}))
    }

    fn registry_with_defaults(config_schema: Value, default_config: Value) -> ModuleRegistry {
        let mut manifest = ModuleManifest::new(ModuleId::new("warn").unwrap(), "Warn", "0.1.0");
        manifest.config_schema = config_schema;
        manifest.default_config = default_config;
        manifest.capabilities = [Capability::SendMessages].into_iter().collect();
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        ModuleRegistry::new(vec![module]).unwrap()
    }

    #[test]
    fn schema_lists_languages_and_embeds_module_schemas() {
        let schema = document_schema(&registry()).unwrap();
        assert_eq!(
            schema["properties"]["language"]["enum"],
            json!(["auto", "en", "ja"])
        );
        let warn = &schema["properties"]["modules"]["properties"]["warn"];
        assert_eq!(
            warn["properties"]["config"]["properties"]["limit"]["anyOf"][0]["type"],
            "integer"
        );
        assert!(warn["properties"]["config"].get("$schema").is_none());
        assert_eq!(
            warn["properties"]["capabilities"]["items"]["enum"],
            json!(["send_messages"])
        );
    }

    #[test]
    fn documents_written_by_the_bot_satisfy_the_published_schema() {
        let registry = registry();
        let schema = document_schema(&registry).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();

        let mut overrides = crate::model::GuildOverrides {
            preset: Some(Preset::Small),
            ..crate::model::GuildOverrides::default()
        };
        overrides.modules.insert(
            "warn".into(),
            ModuleOverride {
                enabled: Some(true),
                config: Some(json!({ "limit": 4 })),
                ..ModuleOverride::default()
            },
        );
        let yaml = overrides_to_yaml(&overrides).unwrap();
        let document: Value = serde_saphyr::from_str(&yaml).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&document)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{errors:?}\n{yaml}");
    }

    #[test]
    fn null_is_accepted_wherever_it_means_use_the_default() {
        let schema = document_schema(&registry()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let config = |value: Value| json!({ "modules": { "warn": { "config": value } } });
        assert!(validator.is_valid(&config(json!({ "limit": null }))));
        assert!(validator.is_valid(&config(json!({ "limit": 3 }))));
        assert!(!validator.is_valid(&config(json!({ "limit": "three" }))));
    }

    #[test]
    fn null_is_accepted_for_properties_declared_inside_combinator_keywords() {
        let schema = document_schema(&registry_with(json!({
            "type": "object",
            "allOf": [{ "properties": { "limit": { "type": "integer", "minimum": 1 } } }],
            "anyOf": [{ "properties": { "a": { "type": "integer" } } }],
            "patternProperties": { "^x_": { "type": "string" } }
        })))
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let config = |value: Value| json!({ "modules": { "warn": { "config": value } } });

        for key in ["limit", "a", "x_name"] {
            assert!(
                validator.is_valid(&config(json!({ key: null }))),
                "{key} should accept null"
            );
        }
        assert!(validator.is_valid(&config(json!({ "limit": 3 }))));
        assert!(!validator.is_valid(&config(json!({ "limit": 0 }))));
        assert!(!validator.is_valid(&config(json!({ "limit": "three" }))));
        assert!(!validator.is_valid(&config(json!({ "x_name": 5 }))));
    }

    #[test]
    fn null_is_accepted_for_properties_declared_inside_then_and_else() {
        // `then`/`else` only count next to an `if`, so the branch that applies is made explicit.
        let schema = document_schema(&registry_with(json!({
            "type": "object",
            "if": { "properties": { "kind": { "const": "a" } }, "required": ["kind"] },
            "then": { "properties": { "b": { "type": "integer" } } },
            "else": { "properties": { "c": { "type": "integer" } } }
        })))
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let config = |value: Value| json!({ "modules": { "warn": { "config": value } } });

        assert!(validator.is_valid(&config(json!({ "kind": "a", "b": null }))));
        assert!(validator.is_valid(&config(json!({ "kind": "a", "b": 1 }))));
        assert!(!validator.is_valid(&config(json!({ "kind": "a", "b": "one" }))));
        assert!(validator.is_valid(&config(json!({ "kind": "z", "c": null }))));
        assert!(!validator.is_valid(&config(json!({ "kind": "z", "c": "one" }))));
    }

    /// A schema whose `if` depends on `mode`, where `then` and `else` bound `limit` differently.
    fn conditional_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "mode": { "enum": ["x", "y"] }, "limit": { "type": "integer" } },
            "if": { "properties": { "mode": { "const": "x" } } },
            "then": { "properties": { "limit": { "maximum": 10 } } },
            "else": { "properties": { "limit": { "minimum": 20 } } }
        })
    }

    /// Whether the compiler accepts this module config for a guild, after `null` was replaced by
    /// the module's default.
    fn compiler_accepts(registry: &Arc<ModuleRegistry>, config: &Value) -> bool {
        let mut overrides = crate::model::GuildOverrides::default();
        overrides.modules.insert(
            "warn".into(),
            ModuleOverride {
                config: Some(config.clone()),
                ..ModuleOverride::default()
            },
        );
        crate::compile::ConfigCompiler::new(Arc::clone(registry))
            .unwrap()
            .compile(&overrides)
            .is_ok()
    }

    #[test]
    fn an_if_condition_picks_the_same_branch_as_the_compiler_does() {
        // Defaults are mode "y", limit 25, so `mode: null` means "y" and `else` applies.
        let registry = Arc::new(registry_with_defaults(
            conditional_schema(),
            json!({ "mode": "y", "limit": 25 }),
        ));
        let schema = document_schema(&registry).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();

        for (config, expected) in [
            (json!({ "mode": null, "limit": 25 }), true),
            (json!({ "mode": null, "limit": 5 }), false),
            (json!({ "mode": "x", "limit": 5 }), true),
            (json!({ "mode": "x", "limit": 25 }), false),
        ] {
            let published = json!({ "modules": { "warn": { "config": config } } });
            assert_eq!(validator.is_valid(&published), expected, "schema: {config}");
            assert_eq!(
                compiler_accepts(&registry, &config),
                expected,
                "compiler: {config}"
            );
        }
    }

    #[test]
    fn known_limit_a_null_under_an_if_condition_is_judged_without_the_default() {
        // Here the default has no `mode`, so `mode: null` leaves it unset and the compiler's `if`
        // succeeds on the missing property (`then`: at most 10). The schema cannot know that and
        // judges the null itself (`if` fails, `else`: at least 20). This is the documented limit.
        let registry = Arc::new(registry_with(conditional_schema()));
        let schema = document_schema(&registry).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let config = json!({ "mode": null, "limit": 25 });
        let published = json!({ "modules": { "warn": { "config": config } } });

        assert!(validator.is_valid(&published));
        assert!(!compiler_accepts(&registry, &config));
    }

    #[test]
    fn one_of_keeps_requiring_exactly_one_matching_branch() {
        let schema = document_schema(&registry_with(json!({
            "type": "object",
            "oneOf": [
                { "properties": { "value": { "type": "integer" } }, "required": ["value"] },
                { "properties": { "value": { "type": "null" } }, "required": ["value"] }
            ]
        })))
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let config = |value: Value| json!({ "modules": { "warn": { "config": value } } });

        // Had both branches been made nullable, `null` would match both and be refused.
        assert!(validator.is_valid(&config(json!({ "value": null }))));
        assert!(validator.is_valid(&config(json!({ "value": 3 }))));
        assert!(!validator.is_valid(&config(json!({ "value": "three" }))));
    }

    #[test]
    fn schema_rejects_what_the_parser_rejects() {
        let schema = document_schema(&registry()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(!validator.is_valid(&json!({ "language": "klingon" })));
        assert!(!validator.is_valid(&json!({ "modules": { "nope": {} } })));
        assert!(validator.is_valid(&json!({ "language": "ja" })));
    }
}
