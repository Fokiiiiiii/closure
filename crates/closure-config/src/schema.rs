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
        let mut manifest = ModuleManifest::new(ModuleId::new("warn").unwrap(), "Warn", "0.1.0");
        manifest.config_schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "limit": { "type": "integer" } }
        });
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
            warn["properties"]["config"]["properties"]["limit"]["type"],
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
    fn schema_rejects_what_the_parser_rejects() {
        let schema = document_schema(&registry()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(!validator.is_valid(&json!({ "language": "klingon" })));
        assert!(!validator.is_valid(&json!({ "modules": { "nope": {} } })));
        assert!(validator.is_valid(&json!({ "language": "ja" })));
    }
}
