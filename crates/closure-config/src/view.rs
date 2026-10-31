//! The simple and the advanced view of a guild's configuration.
//!
//! A setting is *basic* when its schema node carries `"x-ui": { "level": "basic" }`, or when an
//! ancestor node does (the nearest marker wins; the default is advanced). The guild-wide keys in
//! [`BASIC_KEYS`] are basic without a marker, and so is every module's `enabled` switch. A
//! document's basic view holds only those settings; an object is in it when it is basic or holds a
//! basic setting, and a basic node is taken whole.

use closure_core::registry::ModuleRegistry;
use serde_json::{Map, Value};

use crate::error::{ConfigError, Issue};
use crate::model::GuildOverrides;
use crate::yaml::{parse_overrides, parse_value};

/// Which settings a request reads or edits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Advanced,
    Basic,
}

impl View {
    /// `None` for any other name, so that a typo is reported rather than read as the default.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "advanced" => Some(Self::Advanced),
            "basic" => Some(Self::Basic),
            _ => None,
        }
    }
}

/// The guild-wide keys of the simple view. `version` is the format stamp, not a setting: it is
/// accepted in both views so that a document read in one can be written back in the other.
pub const BASIC_KEYS: [&str; 9] = [
    "version",
    "preset",
    "language",
    "log_channel",
    "moderator_roles",
    "timezone",
    "prefix",
    "success_emoji",
    "error_emoji",
];

/// Whether a schema node is marked basic, or `None` when it says nothing about it.
pub(crate) fn marked(schema: &Value) -> Option<bool> {
    schema
        .get("x-ui")?
        .get("level")?
        .as_str()
        .map(|level| level == "basic")
}

/// The part of `value` that is in the basic view of `schema`; `None` when none of it is.
pub(crate) fn basic_part_of(value: &Value, schema: &Value, inherited: bool) -> Option<Value> {
    let basic = marked(schema).unwrap_or(inherited);
    if let (Value::Object(map), Some(properties)) = (value, schema.get("properties")) {
        let properties = properties.as_object()?;
        let mut kept = Map::new();
        for (key, item) in map {
            match properties.get(key) {
                Some(child) => {
                    if let Some(part) = basic_part_of(item, child, basic) {
                        kept.insert(key.clone(), part);
                    }
                }
                None if basic => {
                    kept.insert(key.clone(), item.clone());
                }
                None => {}
            }
        }
        return (basic || !kept.is_empty()).then_some(Value::Object(kept));
    }
    basic.then(|| value.clone())
}

/// The schema of the basic view of `schema`: its basic properties and the containers around them.
/// `None` when nothing in it is basic.
pub(crate) fn basic_schema(schema: &Value, inherited: bool) -> Option<Value> {
    let basic = marked(schema).unwrap_or(inherited);
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return basic.then(|| schema.clone());
    };
    let kept: Map<String, Value> = properties
        .iter()
        .filter_map(|(name, child)| Some((name.clone(), basic_schema(child, basic)?)))
        .collect();
    if !basic && kept.is_empty() {
        return None;
    }
    let mut pruned = schema.as_object()?.clone();
    if let Some(Value::Array(required)) = pruned.get_mut("required") {
        required.retain(|name| name.as_str().is_some_and(|name| kept.contains_key(name)));
    }
    pruned.insert("properties".into(), Value::Object(kept));
    Some(Value::Object(pruned))
}

/// The configuration schema of one module, as the basic view takes it.
pub(crate) fn module_schema<'a>(registry: &'a ModuleRegistry, id: &str) -> Option<&'a Value> {
    let id = closure_core::module::ModuleId::new(id).ok()?;
    registry
        .get(&id)
        .map(|module| &module.manifest().config_schema)
}

/// The basic view of a stored document.
pub fn basic_view(
    overrides: &GuildOverrides,
    registry: &ModuleRegistry,
) -> Result<GuildOverrides, ConfigError> {
    let document = to_value(overrides)?;
    let Some(map) = document.as_object() else {
        return Ok(GuildOverrides::default());
    };
    let mut out = Map::new();
    for key in BASIC_KEYS {
        if let Some(value) = map.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(Value::Object(modules)) = map.get("modules") {
        let mut kept = Map::new();
        for (id, entry) in modules {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            let mut part = Map::new();
            if let Some(enabled) = entry.get("enabled") {
                part.insert("enabled".into(), enabled.clone());
            }
            if let (Some(config), Some(schema)) = (entry.get("config"), module_schema(registry, id))
                && let Some(config) = basic_part_of(config, schema, false)
            {
                part.insert("config".into(), config);
            }
            if !part.is_empty() {
                kept.insert(id.clone(), Value::Object(part));
            }
        }
        if !kept.is_empty() {
            out.insert("modules".into(), Value::Object(kept));
        }
    }
    from_value(Value::Object(out))
}

/// Applies a basic-shaped document (YAML or JSON) on top of the stored one.
///
/// Keys present are set, keys absent keep their stored value, and `null` clears what it is given
/// for, as it does in a full document. A key outside the basic view is refused by name, so no
/// advanced value is ever changed by this path. The result is a whole document, which the caller
/// validates and stores as a full write would be.
pub fn merge_basic(
    stored: &GuildOverrides,
    yaml: &str,
    registry: &ModuleRegistry,
) -> Result<GuildOverrides, ConfigError> {
    // The typed parse gives the line-numbered errors and the size and alias limits, and it reads
    // each scalar as a full document does. The raw parse is kept only for the `null`s, which clear.
    let typed = to_value(&parse_overrides(yaml)?)?;
    let patch = match normalize(parse_value(yaml)?, Some(&typed)) {
        Value::Null => Map::new(),
        Value::Object(map) => map,
        _ => {
            return Err(ConfigError::Invalid(vec![Issue::new(
                "",
                "invalid_document",
                "the document must be a mapping",
            )]));
        }
    };
    let mut merged = to_value(stored)?.as_object().cloned().unwrap_or_default();
    let mut issues = Vec::new();
    for (key, value) in patch {
        if BASIC_KEYS.contains(&key.as_str()) {
            set_or_remove(&mut merged, &key, value);
        } else if key == "modules" {
            merge_modules(&mut merged, value, registry, &mut issues);
        } else {
            issues.push(not_basic(&key));
        }
    }
    if !issues.is_empty() {
        return Err(ConfigError::Invalid(issues));
    }
    from_value(Value::Object(merged))
}

/// The patch with each value taken as the typed parse reads it, so that `"1"` or `"yes"` mean what
/// they mean in a full document. `null` stays `null`: it clears, and the typed form has no trace of it.
fn normalize(raw: Value, typed: Option<&Value>) -> Value {
    match (raw, typed) {
        (Value::Null, _) => Value::Null,
        (Value::Object(raw), Some(Value::Object(typed))) => Value::Object(
            raw.into_iter()
                .map(|(key, item)| {
                    let normalized = normalize(item, typed.get(&key));
                    (key, normalized)
                })
                .collect(),
        ),
        (_, Some(typed)) if !typed.is_null() => typed.clone(),
        (raw, _) => raw,
    }
}

fn merge_modules(
    merged: &mut Map<String, Value>,
    section: Value,
    registry: &ModuleRegistry,
    issues: &mut Vec<Issue>,
) {
    let stored_modules = merged.get("modules").and_then(Value::as_object).cloned();
    let entries = match section {
        Value::Object(entries) => entries,
        // `modules: null` clears the basic part of every module, as `null` does per module.
        Value::Null => stored_modules
            .iter()
            .flat_map(Map::keys)
            .map(|id| (id.clone(), Value::Null))
            .collect(),
        _ => {
            issues.push(Issue::new(
                "modules",
                "invalid_document",
                "must be a mapping",
            ));
            return;
        }
    };
    let mut modules = stored_modules.unwrap_or_default();
    for (id, entry) in entries {
        let path = format!("modules.{id}");
        let entry = match entry {
            Value::Object(entry) => entry,
            Value::Null => Map::from_iter([
                ("enabled".into(), Value::Null),
                ("config".into(), Value::Null),
            ]),
            _ => {
                issues.push(Issue::new(path, "invalid_document", "must be a mapping"));
                continue;
            }
        };
        let mut target = modules
            .get(&id)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (field, value) in entry {
            match field.as_str() {
                "enabled" => set_or_remove(&mut target, "enabled", value),
                "config" => merge_config(&mut target, value, registry, &id, &path, issues),
                _ => issues.push(not_basic(&format!("{path}.{field}"))),
            }
        }
        if target.is_empty() {
            modules.remove(&id);
        } else {
            modules.insert(id, Value::Object(target));
        }
    }
    if modules.is_empty() {
        merged.remove("modules");
    } else {
        merged.insert("modules".into(), Value::Object(modules));
    }
}

fn merge_config(
    target: &mut Map<String, Value>,
    section: Value,
    registry: &ModuleRegistry,
    id: &str,
    path: &str,
    issues: &mut Vec<Issue>,
) {
    let Some(schema) = module_schema(registry, id) else {
        // Unknown modules are reported by the compiler; nothing of theirs is basic.
        issues.push(not_basic(&format!("{path}.config")));
        return;
    };
    let mut config = target
        .get("config")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match section {
        Value::Null => remove_basic(&mut config, schema, false),
        Value::Object(keys) => {
            for (key, value) in keys {
                merge_key(
                    &mut config,
                    &key,
                    value,
                    schema,
                    false,
                    &format!("{path}.config"),
                    issues,
                );
            }
        }
        _ => issues.push(Issue::new(
            format!("{path}.config"),
            "invalid_document",
            "must be a mapping",
        )),
    }
    if config.is_empty() {
        target.remove("config");
    } else {
        target.insert("config".into(), Value::Object(config));
    }
}

/// Sets one key of a module's config, when that key is basic (or holds basic settings below it).
fn merge_key(
    target: &mut Map<String, Value>,
    key: &str,
    value: Value,
    schema: &Value,
    inherited: bool,
    path: &str,
    issues: &mut Vec<Issue>,
) {
    let path = format!("{path}.{key}");
    let child = schema.get("properties").and_then(|props| props.get(key));
    let Some(child) = child else {
        // Keys the schema does not name are not part of the basic view.
        issues.push(not_basic(&path));
        return;
    };
    if marked(child).unwrap_or(inherited) {
        set_or_remove(target, key, value);
    } else if basic_schema(child, false).is_some() {
        match value {
            Value::Object(keys) => {
                let mut nested = target
                    .get(key)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                for (inner, inner_value) in keys {
                    merge_key(
                        &mut nested,
                        &inner,
                        inner_value,
                        child,
                        false,
                        &path,
                        issues,
                    );
                }
                if nested.is_empty() {
                    target.remove(key);
                } else {
                    target.insert(key.into(), Value::Object(nested));
                }
            }
            _ => issues.push(not_basic(&path)),
        }
    } else {
        issues.push(not_basic(&path));
    }
}

/// Removes the basic settings of an object, keeping its advanced ones.
fn remove_basic(target: &mut Map<String, Value>, schema: &Value, inherited: bool) {
    let basic = marked(schema).unwrap_or(inherited);
    if basic {
        target.clear();
        return;
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let keys: Vec<String> = target.keys().cloned().collect();
    for key in keys {
        let Some(child) = properties.get(&key) else {
            continue;
        };
        if marked(child).unwrap_or(false) {
            target.remove(&key);
        } else if let Some(nested) = target.get_mut(&key).and_then(Value::as_object_mut) {
            remove_basic(nested, child, false);
            if nested.is_empty() {
                target.remove(&key);
            }
        }
    }
}

fn set_or_remove(target: &mut Map<String, Value>, key: &str, value: Value) {
    if value.is_null() {
        target.remove(key);
    } else {
        target.insert(key.into(), value);
    }
}

fn not_basic(path: &str) -> Issue {
    Issue::new(
        path,
        "not_basic",
        format!("`{path}` is not part of the basic view; send it with the full document"),
    )
}

fn to_value(overrides: &GuildOverrides) -> Result<Value, ConfigError> {
    serde_json::to_value(overrides).map_err(|error| ConfigError::Serialize(error.to_string()))
}

fn from_value(value: Value) -> Result<GuildOverrides, ConfigError> {
    serde_json::from_value(value).map_err(|error| {
        ConfigError::Invalid(vec![Issue::new("", "invalid_value", error.to_string())])
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::capability::Capability;
    use closure_core::module::{Module, ModuleId, ModuleManifest};
    use closure_core::permission::{Level, PermissionSpec};
    use closure_core::registry::ModuleRegistry;
    use closure_core::testing::StubModule;
    use serde_json::json;

    use super::*;
    use crate::yaml::overrides_to_yaml;

    fn registry() -> ModuleRegistry {
        let mut manifest = ModuleManifest::new(ModuleId::new("warn").unwrap(), "Warn", "0.1.0");
        manifest.config_schema = json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "x-ui": { "level": "basic" } },
                "words": { "type": "array", "items": { "type": "string" } }
            },
            "additionalProperties": false
        });
        manifest.default_config = json!({ "limit": 3, "words": [] });
        manifest.capabilities = [Capability::SendMessages].into_iter().collect();
        manifest.permissions =
            vec![PermissionSpec::new("warn.add", Level::MODERATOR, "add warnings").unwrap()];
        let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
        ModuleRegistry::new(vec![module]).unwrap()
    }

    fn stored(yaml: &str) -> GuildOverrides {
        crate::yaml::parse_overrides(yaml).unwrap()
    }

    fn value(overrides: &GuildOverrides) -> Value {
        serde_json::to_value(overrides).unwrap()
    }

    #[test]
    fn the_view_names_are_exact() {
        assert_eq!(View::parse("basic"), Some(View::Basic));
        assert_eq!(View::parse("advanced"), Some(View::Advanced));
        assert_eq!(View::parse("Basic"), None);
        assert_eq!(View::parse(""), None);
    }

    #[test]
    fn the_basic_view_keeps_the_basic_keys_and_the_marked_module_settings() {
        let document = stored(
            "language: ja\npreset: large\nlimits:\n  concurrency: 12\n\
             permissions:\n  roles: { 1: moderator }\n\
             modules:\n  warn:\n    enabled: true\n    config: { limit: 7, words: [a] }\n",
        );
        let basic = value(&basic_view(&document, &registry()).unwrap());
        assert_eq!(
            basic,
            json!({
                "language": "ja",
                "preset": "large",
                "modules": { "warn": { "enabled": true, "config": { "limit": 7 } } }
            })
        );
    }

    #[test]
    fn a_module_with_nothing_basic_is_left_out_except_its_switch() {
        let document = stored("modules:\n  warn:\n    capabilities: [send_messages]\n");
        assert_eq!(
            value(&basic_view(&document, &registry()).unwrap()),
            json!({})
        );
    }

    #[test]
    fn a_basic_merge_sets_leaves_and_clears_only_what_it_names() {
        let base = stored(
            "limits:\n  concurrency: 12\nlanguage: ja\nprefix: '!'\n\
             modules:\n  warn:\n    enabled: true\n    config: { limit: 7, words: [a] }\n",
        );
        let merged = merge_basic(
            &base,
            "language: en\nprefix: null\nmodules:\n  warn:\n    config: { limit: 4 }\n",
            &registry(),
        )
        .unwrap();
        let merged = value(&merged);
        assert_eq!(merged["language"], "en");
        assert!(merged.get("prefix").is_none(), "{merged}");
        assert_eq!(merged["limits"]["concurrency"], 12, "advanced kept");
        assert_eq!(
            merged["modules"]["warn"]["enabled"], true,
            "absent keys kept"
        );
        assert_eq!(merged["modules"]["warn"]["config"]["limit"], 4);
        assert_eq!(
            merged["modules"]["warn"]["config"]["words"],
            json!(["a"]),
            "advanced kept"
        );
    }

    #[test]
    fn a_basic_merge_refuses_advanced_keys_by_name() {
        let base = stored("limits:\n  concurrency: 12\n");
        let error = merge_basic(&base, "limits:\n  concurrency: 2\n", &registry()).unwrap_err();
        let issues = error.issues();
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].path, "limits");
        assert_eq!(issues[0].code, "not_basic");

        let error = merge_basic(
            &base,
            "modules:\n  warn:\n    config: { words: [x] }\n",
            &registry(),
        )
        .unwrap_err();
        assert_eq!(error.issues()[0].path, "modules.warn.config.words");
    }

    #[test]
    fn a_basic_merge_takes_the_format_stamp_and_reports_bad_syntax_with_a_line() {
        let base = GuildOverrides::default();
        let merged = merge_basic(&base, "version: 1\npreset: small\n", &registry()).unwrap();
        assert_eq!(merged.version, Some(1));
        let error = merge_basic(&base, "preset: small\npreest: x\n", &registry()).unwrap_err();
        assert!(
            matches!(error, ConfigError::Syntax { line: Some(2), .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_null_module_entry_clears_its_basic_part_and_keeps_the_rest() {
        let base = stored(
            "modules:\n  warn:\n    enabled: true\n    capabilities: [send_messages]\n    config: { limit: 7 }\n",
        );
        let merged = value(&merge_basic(&base, "modules:\n  warn: null\n", &registry()).unwrap());
        assert!(
            merged["modules"]["warn"].get("enabled").is_none(),
            "{merged}"
        );
        assert!(
            merged["modules"]["warn"]["config"].get("limit").is_none(),
            "{merged}"
        );
        assert_eq!(
            merged["modules"]["warn"]["capabilities"],
            json!(["send_messages"])
        );
    }

    #[test]
    fn a_basic_merge_result_is_an_ordinary_document() {
        let base = stored("preset: small\n");
        let merged = merge_basic(&base, "language: ja\n", &registry()).unwrap();
        let yaml = overrides_to_yaml(&merged).unwrap();
        // The YAML writer stamps the format version; everything else round-trips.
        let mut expected = merged.clone();
        expected.version = Some(crate::model::CURRENT_VERSION);
        assert_eq!(crate::yaml::parse_overrides(&yaml).unwrap(), expected);
    }

    #[test]
    fn the_basic_schema_of_a_node_drops_required_names_it_no_longer_has() {
        let schema = json!({
            "type": "object",
            "required": ["inner", "other"],
            "properties": {
                "inner": { "type": "integer", "x-ui": { "level": "basic" } },
                "other": { "type": "integer" }
            }
        });
        let basic = basic_schema(&schema, false).unwrap();
        assert_eq!(basic["required"], json!(["inner"]));
    }

    #[test]
    fn a_basic_merge_reads_each_scalar_as_the_full_document_does() {
        let yaml = "version: \"1\"\nlog_channel: \"123456789012345678\"\nmoderator_roles: [\"1\", 2]\n\
                    modules:\n  warn:\n    enabled: \"yes\"\n    config: { limit: 4 }\n";
        let basic = merge_basic(&GuildOverrides::default(), yaml, &registry()).unwrap();
        let full = crate::yaml::parse_overrides(yaml).unwrap();
        assert_eq!(value(&basic), value(&full));
    }
}
