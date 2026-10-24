use std::collections::{BTreeMap, BTreeSet};

use closure_core::capability::Capability;
use closure_core::locale::Locale;
use closure_core::registry::ModuleRegistry;
use serde_json::{Map, Value, json};

use crate::error::ConfigError;
use crate::model::GuildOverrides;

/// The JSON Schema of the whole configuration document, with every registered module's own
/// config schema embedded. Dashboards generate their forms from it and editors use it for
/// completion; it is derived from the same types the parser uses, so it cannot drift.
///
/// A document holds only what differs from the defaults, so what is published is the schema of
/// such an *override*, made from each module's own schema by [`relax`]. Its contract: **it never
/// rejects an override that `ConfigCompiler` accepts.** It may accept one the compiler then
/// refuses, because the compiler judges the result of merging the override into the defaults
/// and the schema only sees the override.
pub fn document_schema(registry: &ModuleRegistry) -> Result<Value, ConfigError> {
    let mut schema = serde_json::to_value(schemars::schema_for!(GuildOverrides))
        .map_err(|error| ConfigError::Serialize(error.to_string()))?;

    let mut languages = vec![json!("auto")];
    languages.extend(Locale::ALL.iter().map(|locale| json!(locale.tag())));

    let mut module_properties = Map::new();
    for module in registry.iter() {
        let manifest = module.manifest();
        check_supported(&manifest.config_schema).map_err(|message| {
            ConfigError::BadModuleSchema {
                module: manifest.id.to_string(),
                message,
            }
        })?;
        let config_schema =
            published_config_schema(&manifest.id.to_string(), &manifest.config_schema);
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

/// A module's config schema as it is published inside the document.
fn published_config_schema(module: &str, schema: &Value) -> Value {
    let mut config = schema.clone();
    if let Value::Object(map) = &mut config {
        map.remove("$schema");
        // A reference scope of its own: `#/$defs/..` in a module's schema means that schema's own
        // definitions, not those of the document it is embedded in or of another module. A
        // schema that names itself keeps its name.
        map.entry("$id")
            .or_insert_with(|| json!(format!("urn:closure:module:{module}")));
    }
    relax(&mut config);
    // `config: null` stands for "the defaults" as well.
    json!({ "anyOf": [config, { "type": "null" }] })
}

const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Where a schema keeps definitions that `$ref` can point to.
const DEFINITIONS: [&str; 2] = ["$defs", "definitions"];

/// Keywords whose values are schemas, by name.
const SCHEMA_MAPS: [&str; 5] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];

/// Keywords whose values are data, not schemas.
const DATA: [&str; 4] = ["const", "enum", "default", "examples"];

/// Keywords for references that cannot be followed by name, so the schema cannot be translated.
const UNSUPPORTED: [&str; 5] = [
    "$anchor",
    "$dynamicAnchor",
    "$dynamicRef",
    "$recursiveAnchor",
    "$recursiveRef",
];

/// Keywords that judge which keys an object has, or the object as a whole. A sparse override
/// lacks keys the defaults fill in, and weakening what sits under `not` makes it stricter, so
/// these are dropped where values are merged.
const DROPPED: [&str; 5] = [
    "required",
    "dependentRequired",
    "dependentSchemas",
    "minProperties",
    "not",
];

/// How deep [`relax`] follows nesting and references. What is deeper stays as written, which is
/// strict, so a schema this deep may reject a sparse document that the compiler accepts.
const MAX_DEPTH: usize = 128;

/// Whether a module's config schema is one [`relax`] can translate faithfully. Checked when the
/// bot starts, so that a schema it could only publish wrongly is refused with a reason instead.
pub(crate) fn check_supported(schema: &Value) -> Result<(), String> {
    if let Some(declared) = schema.get("$schema")
        && declared
            .as_str()
            .is_none_or(|uri| uri.trim_end_matches('#') != DRAFT_2020_12)
    {
        return Err(format!(
            "only JSON Schema draft 2020-12 is supported, not {declared}"
        ));
    }
    scan(schema)
}

fn scan(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for (keyword, inner) in map {
                if UNSUPPORTED.contains(&keyword.as_str()) {
                    return Err(format!("`{keyword}` is not supported"));
                }
                if keyword == "$ref"
                    && let Value::String(target) = inner
                    && target != "#"
                    && local_definition(target).is_none()
                {
                    return Err(format!(
                        "`$ref` to `{target}` is not supported; use `#`, `#/$defs/<name>` or `#/definitions/<name>`"
                    ));
                }
                if DATA.contains(&keyword.as_str()) {
                    continue;
                }
                if SCHEMA_MAPS.contains(&keyword.as_str()) {
                    if let Value::Object(named) = inner {
                        named.values().try_for_each(scan)?;
                    }
                } else {
                    scan(inner)?;
                }
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(scan),
        _ => Ok(()),
    }
}

/// Turns the schema of a configuration into the schema of an override of it.
///
/// Values are merged key by key into the defaults, `null` meaning "the default", and arrays are
/// replaced whole. So, wherever values are merged: a property may be `null`, keywords that look
/// at which keys exist are dropped, `oneOf` and `if`/`then`/`else` are weakened to what any of
/// their branches allows, and an unknown key may only be `null` (the merge drops it). A property
/// may be a type defined elsewhere in the schema (`$ref` to `$defs`); a reference reached the way
/// a property is gets a copy of the definition made the same way, and the original stays as it
/// is for the places that need it strict, such as the items of a list.
fn relax(schema: &mut Value) {
    let mut copies = Copies::new(schema);
    copies.visit(schema, 0, true);
    copies.add_to(schema);
}

struct Copies {
    /// The definitions of the schema, by `(where they are kept, name)`.
    originals: BTreeMap<(&'static str, String), Value>,
    /// Names in use: the definitions, and the copies made so far.
    taken: BTreeSet<(&'static str, String)>,
    /// The name of the copy of each definition.
    names: BTreeMap<(&'static str, String), String>,
    /// The copies, by `(where they are kept, name of the copy)`.
    made: BTreeMap<(&'static str, String), Value>,
}

impl Copies {
    fn new(root: &Value) -> Self {
        let mut originals = BTreeMap::new();
        for place in DEFINITIONS {
            if let Some(Value::Object(definitions)) = root.get(place) {
                for (name, definition) in definitions {
                    originals.insert((place, name.clone()), definition.clone());
                }
            }
        }
        Self {
            taken: originals.keys().cloned().collect(),
            originals,
            names: BTreeMap::new(),
            made: BTreeMap::new(),
        }
    }

    fn visit(&mut self, schema: &mut Value, depth: usize, is_root: bool) {
        let Value::Object(map) = schema else { return };
        // Another schema resource: its references mean its own definitions.
        if depth > MAX_DEPTH || (!is_root && map.contains_key("$id")) {
            return;
        }
        for keyword in DROPPED {
            map.remove(keyword);
        }
        fold_conditionals(map);
        self.redirect_reference(map, depth);
        for keyword in ["properties", "patternProperties"] {
            if let Some(Value::Object(properties)) = map.get_mut(keyword) {
                for sub in properties.values_mut() {
                    self.nullable(sub, depth);
                }
            }
        }
        match map.get_mut("additionalProperties") {
            Some(additional @ Value::Object(_)) => self.nullable(additional, depth),
            // No keys but the declared ones; a `null` for an unknown key is dropped by the merge.
            Some(forbidden @ Value::Bool(false)) => *forbidden = json!({ "type": "null" }),
            _ => {}
        }
        for keyword in ["allOf", "anyOf"] {
            if let Some(Value::Array(branches)) = map.get_mut(keyword) {
                for branch in branches {
                    self.visit(branch, depth + 1, false);
                }
            }
        }
    }

    /// Lets `null` stand in for the value of a property.
    fn nullable(&mut self, sub: &mut Value, depth: usize) {
        self.visit(sub, depth + 1, false);
        let inner = sub.take();
        *sub = json!({ "anyOf": [inner, { "type": "null" }] });
    }

    /// Points a `$ref` to a definition of this schema at the copy of it, making the copy first if
    /// there is none yet.
    fn redirect_reference(&mut self, map: &mut Map<String, Value>, depth: usize) {
        let Some(Value::String(target)) = map.get("$ref") else {
            return;
        };
        let Some((place, name)) = local_definition(target) else {
            return;
        };
        let key = (place, name);
        let Some(original) = self.originals.get(&key).cloned() else {
            return;
        };
        let copy_name = if let Some(existing) = self.names.get(&key) {
            existing.clone()
        } else {
            let mut candidate = format!("{}__nullable", key.1);
            while self.taken.contains(&(place, candidate.clone())) {
                candidate.push('_');
            }
            self.taken.insert((place, candidate.clone()));
            self.names.insert(key.clone(), candidate.clone());
            // A placeholder first: a type that refers to itself finds the copy being made.
            self.made.insert((place, candidate.clone()), Value::Null);
            let mut copy = original;
            self.visit(&mut copy, depth + 1, false);
            self.made.insert((place, candidate.clone()), copy);
            candidate
        };
        map.insert("$ref".into(), json!(format!("#/{place}/{copy_name}")));
    }

    fn add_to(self, root: &mut Value) {
        for ((place, name), copy) in self.made {
            if let Some(Value::Object(definitions)) = root.get_mut(place) {
                definitions.insert(name, copy);
            }
        }
    }
}

/// Weakens `oneOf` and `if`/`then`/`else` to "any of the branches", as `allOf` entries, because a
/// sparse override cannot say which branch applies once the defaults are filled in. `if` and a
/// `then` or `else` without its partner say nothing the override can be held to, and are dropped.
fn fold_conditionals(map: &mut Map<String, Value>) {
    let mut alternatives = Vec::new();
    if let Some(Value::Array(branches)) = map.remove("oneOf") {
        alternatives.push(json!({ "anyOf": branches }));
    }
    map.remove("if");
    if let (Some(then), Some(otherwise)) = (map.remove("then"), map.remove("else")) {
        alternatives.push(json!({ "anyOf": [then, otherwise] }));
    }
    if alternatives.is_empty() {
        return;
    }
    if let Value::Array(all) = map.entry("allOf").or_insert_with(|| json!([])) {
        all.extend(alternatives);
    }
}

/// `(where, name)` of a `$ref` like `#/$defs/Name` that points into the schema itself. Names that
/// need escaping in a JSON pointer are not handled.
fn local_definition(reference: &str) -> Option<(&'static str, String)> {
    DEFINITIONS.into_iter().find_map(|place| {
        let name = reference.strip_prefix(&format!("#/{place}/"))?;
        (!name.contains(['/', '~', '%'])).then(|| (place, name.to_owned()))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use closure_core::capability::Capability;
    use closure_core::module::{Module, ModuleId, ModuleManifest};
    use closure_core::testing::StubModule;

    use super::*;
    use crate::compile::ConfigCompiler;
    use crate::model::{GuildOverrides, ModuleOverride, Preset};
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

    /// A document that sets the `warn` module's config to `config`.
    fn document(config: &Value) -> Value {
        json!({ "modules": { "warn": { "config": config } } })
    }

    fn validator_of(registry: &ModuleRegistry) -> jsonschema::Validator {
        jsonschema::validator_for(&document_schema(registry).unwrap())
            .expect("the published schema must build")
    }

    /// Whether the compiler accepts this config of the `warn` module, after merging it into the
    /// defaults.
    fn compiler_accepts(registry: &Arc<ModuleRegistry>, config: &Value) -> bool {
        let mut overrides = GuildOverrides::default();
        overrides.modules.insert(
            "warn".into(),
            ModuleOverride {
                config: Some(config.clone()),
                ..ModuleOverride::default()
            },
        );
        ConfigCompiler::new(Arc::clone(registry))
            .unwrap()
            .compile(&overrides)
            .is_ok()
    }

    /// The contract of the published schema: it never rejects what the compiler accepts. Each
    /// of `configs` must be accepted by the compiler (or the test proves nothing) and so by the
    /// schema.
    fn assert_never_rejects_what_the_compiler_accepts(
        registry: &Arc<ModuleRegistry>,
        configs: &[Value],
    ) {
        let validator = validator_of(registry);
        for config in configs {
            assert!(compiler_accepts(registry, config), "test setup: {config}");
            assert!(
                validator.is_valid(&document(config)),
                "the published schema rejects what the compiler accepts: {config}"
            );
        }
    }

    #[test]
    fn schema_lists_languages_and_embeds_module_schemas() {
        let schema = document_schema(&registry()).unwrap();
        assert_eq!(
            schema["properties"]["language"]["enum"],
            json!(["auto", "en", "ja"])
        );
        let warn = &schema["properties"]["modules"]["properties"]["warn"];
        let config = &warn["properties"]["config"]["anyOf"][0];
        assert_eq!(config["properties"]["limit"]["anyOf"][0]["type"], "integer");
        assert!(config.get("$schema").is_none());
        assert_eq!(
            warn["properties"]["capabilities"]["items"]["enum"],
            json!(["send_messages"])
        );
    }

    #[test]
    fn documents_written_by_the_bot_satisfy_the_published_schema() {
        let registry = registry();
        let validator = validator_of(&registry);

        let mut overrides = GuildOverrides {
            preset: Some(Preset::Small),
            ..GuildOverrides::default()
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
        let validator = validator_of(&registry());
        assert!(validator.is_valid(&document(&json!({ "limit": null }))));
        assert!(validator.is_valid(&document(&json!({ "limit": 3 }))));
        assert!(!validator.is_valid(&document(&json!({ "limit": "three" }))));
    }

    #[test]
    fn the_whole_config_may_be_null_too() {
        let validator = validator_of(&registry());
        assert!(validator.is_valid(&document(&Value::Null)));
        assert!(!validator.is_valid(&document(&json!("text"))));
    }

    #[test]
    fn null_is_accepted_for_properties_declared_inside_combinator_keywords() {
        let registry = registry_with(json!({
            "type": "object",
            "allOf": [{ "properties": { "limit": { "type": "integer", "minimum": 1 } } }],
            "anyOf": [{ "properties": { "a": { "type": "integer" } } }],
            "patternProperties": { "^x_": { "type": "string" } }
        }));
        let validator = validator_of(&registry);

        for key in ["limit", "a", "x_name"] {
            assert!(
                validator.is_valid(&document(&json!({ key: null }))),
                "{key} should accept null"
            );
        }
        assert!(validator.is_valid(&document(&json!({ "limit": 3 }))));
        assert!(!validator.is_valid(&document(&json!({ "limit": 0 }))));
        assert!(!validator.is_valid(&document(&json!({ "limit": "three" }))));
        assert!(!validator.is_valid(&document(&json!({ "x_name": 5 }))));
    }

    #[test]
    fn keywords_that_look_at_which_keys_exist_are_dropped() {
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "required": ["a", "b"],
                "minProperties": 2,
                "dependentRequired": { "a": ["b"] },
                "dependentSchemas": { "a": { "required": ["b"] } },
                "not": { "required": ["forbidden"] }
            }),
            json!({ "a": 1, "b": 2 }),
        ));
        // Each leaves out something the defaults fill in.
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[
                json!({}),
                json!({ "a": 3 }),
                json!({ "b": 4 }),
                json!({ "a": null }),
            ],
        );
        let schema = document_schema(&registry).unwrap();
        let config = &schema["properties"]["modules"]["properties"]["warn"]["properties"]["config"]
            ["anyOf"][0];
        for keyword in DROPPED {
            assert!(config.get(keyword).is_none(), "{keyword} is still there");
        }
    }

    #[test]
    fn conditionals_never_reject_what_the_compiler_accepts() {
        // With the defaults mode "y" and limit 25, `else` (at least 20) applies unless mode is "x".
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "properties": { "mode": { "enum": ["x", "y"] }, "limit": { "type": "integer" } },
                "if": { "properties": { "mode": { "const": "x" } } },
                "then": { "properties": { "limit": { "maximum": 10 } } },
                "else": { "properties": { "limit": { "minimum": 20 } } }
            }),
            json!({ "mode": "y", "limit": 25 }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[
                json!({}),
                // The omission of `mode` is what a schema alone cannot judge.
                json!({ "limit": 25 }),
                json!({ "limit": 30 }),
                json!({ "mode": "x", "limit": 5 }),
                json!({ "mode": null, "limit": 25 }),
            ],
        );
        let validator = validator_of(&registry);
        assert!(!validator.is_valid(&document(&json!({ "limit": "many" }))));
        assert!(!validator.is_valid(&document(&json!({ "mode": "z" }))));
    }

    #[test]
    fn one_of_never_rejects_what_the_compiler_accepts() {
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "oneOf": [
                    { "properties": { "kind": { "const": "a" } }, "required": ["kind"] },
                    { "properties": { "kind": { "const": "b" } }, "required": ["kind"] }
                ]
            }),
            json!({ "kind": "a" }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[
                json!({}),
                json!({ "kind": null }),
                json!({ "kind": "a" }),
                json!({ "kind": "b" }),
            ],
        );
        let validator = validator_of(&registry);
        assert!(!validator.is_valid(&document(&json!({ "kind": "c" }))));
    }

    #[test]
    fn an_unknown_key_is_refused_unless_it_is_null() {
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "additionalProperties": false
            }),
            json!({ "a": 1 }),
        ));
        // The merge drops a `null` for a key nobody declared.
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[json!({ "unknown": null }), json!({ "a": 2 })],
        );
        let validator = validator_of(&registry);
        assert!(!validator.is_valid(&document(&json!({ "unknown": 1 }))));
        assert!(!compiler_accepts(&registry, &json!({ "unknown": 1 })));
    }

    /// A module schema as `schemars` writes it: a nested type behind `$ref`, once as a property
    /// and once as the items of a list.
    fn schema_with_definitions() -> Value {
        json!({
            "type": "object",
            "properties": {
                "inner": { "$ref": "#/$defs/Inner" },
                "list": { "type": "array", "items": { "$ref": "#/$defs/Inner" } }
            },
            "$defs": {
                "Inner": {
                    "type": "object",
                    "properties": { "a": { "type": "integer", "minimum": 1 } }
                }
            }
        })
    }

    #[test]
    fn module_schemas_with_definitions_resolve_in_the_published_document() {
        let validator = validator_of(&registry_with(schema_with_definitions()));

        assert!(validator.is_valid(&document(&json!({ "inner": { "a": 3 } }))));
        assert!(!validator.is_valid(&document(&json!({ "inner": { "a": 0 } }))));
        assert!(!validator.is_valid(&document(&json!({ "inner": { "a": "x" } }))));
    }

    #[test]
    fn null_resets_a_property_of_a_nested_type_but_not_an_item_of_a_list() {
        let validator = validator_of(&registry_with(schema_with_definitions()));

        // The nested object is merged key by key, so `null` there means "the default".
        assert!(validator.is_valid(&document(&json!({ "inner": { "a": null } }))));
        assert!(validator.is_valid(&document(&json!({ "inner": null }))));
        // A list is replaced as a whole, so `null` inside an item has no meaning.
        assert!(!validator.is_valid(&document(&json!({ "list": [{ "a": null }] }))));
        assert!(validator.is_valid(&document(&json!({ "list": [{ "a": 3 }] }))));
    }

    #[test]
    fn a_type_that_refers_to_itself_is_made_nullable_without_looping() {
        let validator = validator_of(&registry_with(json!({
            "type": "object",
            "properties": { "root": { "$ref": "#/$defs/Node" } },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "value": { "type": "integer" },
                        "child": { "$ref": "#/$defs/Node" }
                    }
                }
            }
        })));

        let deep = json!({ "root": { "child": { "child": { "value": null } } } });
        assert!(validator.is_valid(&document(&deep)));
        assert!(!validator.is_valid(&document(&json!({ "root": { "child": { "value": "x" } } }))));
    }

    #[test]
    fn a_reference_to_the_root_follows_where_it_is_used() {
        // `Vec<Self>` in a root type: schemars writes `{"$ref": "#"}` for the items.
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "required": ["value", "children"],
                "properties": {
                    "value": { "type": "integer" },
                    "children": { "type": "array", "items": { "$ref": "#" } }
                }
            }),
            json!({ "value": 1, "children": [] }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[
                json!({}),
                json!({ "value": 2 }),
                json!({ "children": [{ "value": 2, "children": [] }] }),
            ],
        );
        let validator = validator_of(&registry);
        assert!(!validator.is_valid(&document(&json!({ "children": [{ "value": "x" }] }))));
    }

    #[test]
    fn two_modules_may_define_a_type_of_the_same_name_differently() {
        let module = |id: &str, field_type: &str| {
            let mut manifest = ModuleManifest::new(ModuleId::new(id).unwrap(), id, "0.1.0");
            manifest.config_schema = json!({
                "type": "object",
                "properties": { "inner": { "$ref": "#/$defs/Inner" } },
                "$defs": { "Inner": { "type": "object", "properties": { "a": { "type": field_type } } } }
            });
            let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
            module
        };
        let registry =
            ModuleRegistry::new(vec![module("first", "integer"), module("second", "string")])
                .unwrap();
        let validator = validator_of(&registry);
        let config =
            |module: &str, value: Value| json!({ "modules": { module: { "config": value } } });

        assert!(validator.is_valid(&config("first", json!({ "inner": { "a": 1 } }))));
        assert!(!validator.is_valid(&config("first", json!({ "inner": { "a": "x" } }))));
        assert!(validator.is_valid(&config("second", json!({ "inner": { "a": "x" } }))));
        assert!(!validator.is_valid(&config("second", json!({ "inner": { "a": 1 } }))));
    }

    #[test]
    fn the_name_of_a_copy_never_takes_the_name_of_a_definition() {
        // The schema already has a definition called `A__nullable`.
        let registry = registry_with(json!({
            "type": "object",
            "properties": {
                "x": { "$ref": "#/$defs/A" },
                "xs": { "type": "array", "items": { "$ref": "#/$defs/A__nullable" } }
            },
            "$defs": { "A": { "type": "integer" }, "A__nullable": { "type": "string" } }
        }));
        let validator = validator_of(&registry);

        assert!(validator.is_valid(&document(&json!({ "xs": ["ok"] }))));
        assert!(!validator.is_valid(&document(&json!({ "xs": [1] }))));
        assert!(validator.is_valid(&document(&json!({ "x": 1 }))));
        assert!(validator.is_valid(&document(&json!({ "x": null }))));
        assert!(!validator.is_valid(&document(&json!({ "x": "text" }))));
    }

    #[test]
    fn a_nested_schema_resource_keeps_its_own_definitions() {
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "$defs": { "X": { "type": "integer" } },
                "properties": {
                    "child": {
                        "$id": "urn:example:child",
                        "type": "object",
                        "$defs": { "X": { "type": "string" } },
                        "properties": { "v": { "$ref": "#/$defs/X" } }
                    }
                }
            }),
            json!({ "child": { "v": "ok" } }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[json!({}), json!({ "child": { "v": "new" } })],
        );
        let validator = validator_of(&registry);
        assert!(!validator.is_valid(&document(&json!({ "child": { "v": 1 } }))));
    }

    #[test]
    fn a_schema_that_names_itself_keeps_its_name() {
        let registry = registry_with(json!({
            "$id": "urn:example:config",
            "type": "object",
            "properties": { "x": { "$ref": "#/$defs/X" } },
            "$defs": { "X": { "type": "integer" } }
        }));
        let schema = document_schema(&registry).unwrap();
        let config = &schema["properties"]["modules"]["properties"]["warn"]["properties"]["config"]
            ["anyOf"][0];
        assert_eq!(config["$id"], "urn:example:config");
        let validator = validator_of(&registry);
        assert!(validator.is_valid(&document(&json!({ "x": null }))));
        assert!(!validator.is_valid(&document(&json!({ "x": "text" }))));
    }

    #[test]
    fn a_very_long_chain_of_definitions_is_followed_only_so_far() {
        let mut definitions = Map::new();
        let length = 400;
        for n in 0..length {
            definitions.insert(
                format!("D{n}"),
                json!({ "type": "object", "properties": { "child": { "$ref": format!("#/$defs/D{}", n + 1) } } }),
            );
        }
        definitions.insert(format!("D{length}"), json!({ "type": "object" }));
        let registry = registry_with(json!({
            "type": "object",
            "properties": { "root": { "$ref": "#/$defs/D0" } },
            "$defs": definitions
        }));
        let validator = validator_of(&registry);
        assert!(validator.is_valid(&document(&json!({ "root": { "child": null } }))));
    }

    #[test]
    fn constructs_that_cannot_be_translated_are_refused_with_a_reason() {
        for (schema, wanted) in [
            (json!({ "$anchor": "here" }), "$anchor"),
            (
                json!({ "properties": { "a": { "$dynamicRef": "#x" } } }),
                "$dynamicRef",
            ),
            (json!({ "$recursiveRef": "#" }), "$recursiveRef"),
            (
                json!({ "$ref": "urn:example:config#/$defs/X" }),
                "urn:example:config#/$defs/X",
            ),
            (json!({ "$ref": "#/$defs/a~1b" }), "a~1b"),
            (
                json!({ "$ref": "#/$defs/A/properties/x" }),
                "A/properties/x",
            ),
            (
                json!({ "$schema": "http://json-schema.org/draft-07/schema#" }),
                "2020-12",
            ),
        ] {
            let error = check_supported(&schema).expect_err(&schema.to_string());
            assert!(error.contains(wanted), "{error}");
        }
    }

    #[test]
    fn what_can_be_translated_is_accepted() {
        for schema in [
            json!({ "$schema": "https://json-schema.org/draft/2020-12/schema" }),
            json!({ "$schema": "https://json-schema.org/draft/2020-12/schema#" }),
            json!({ "properties": { "a": { "$ref": "#" } } }),
            json!({ "properties": { "a": { "$ref": "#/$defs/A" } }, "$defs": { "A": {} } }),
            json!({ "properties": { "a": { "$ref": "#/definitions/A" } }, "definitions": { "A": {} } }),
            // Data that happens to look like keywords is data.
            json!({ "default": { "$anchor": "x", "$ref": "elsewhere" }, "enum": [{ "$dynamicRef": "y" }] }),
            // A property is not a keyword, whatever it is called.
            json!({ "properties": { "$anchor": { "type": "string" }, "$ref": { "type": "string" } } }),
        ] {
            check_supported(&schema).unwrap_or_else(|error| panic!("{schema}: {error}"));
        }
    }

    #[test]
    fn the_bot_does_not_start_with_a_schema_it_could_only_publish_wrongly() {
        let registry = registry_with(json!({ "type": "object", "$anchor": "here" }));
        let Err(error) = ConfigCompiler::new(Arc::new(registry)) else {
            panic!("the schema must be refused");
        };
        assert!(
            matches!(&error, ConfigError::BadModuleSchema { module, message }
                if module == "warn" && message.contains("$anchor")),
            "{error:?}"
        );
    }

    /// What a module author writes with `schemars`: nested types, an optional one and a list.
    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Nested {
        a: u32,
        b: String,
    }

    #[derive(schemars::JsonSchema)]
    #[allow(dead_code)]
    struct AuthorsConfig {
        limit: u32,
        nested: Nested,
        maybe: Option<Nested>,
        items: Vec<Nested>,
    }

    #[test]
    fn a_schema_written_by_schemars_never_rejects_what_the_compiler_accepts() {
        let registry = Arc::new(registry_with_defaults(
            serde_json::to_value(schemars::schema_for!(AuthorsConfig)).unwrap(),
            json!({
                "limit": 3,
                "nested": { "a": 1, "b": "x" },
                "maybe": null,
                "items": []
            }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[
                json!({}),
                json!({ "limit": 5 }),
                json!({ "limit": null }),
                json!({ "nested": { "a": 2 } }),
                json!({ "nested": { "a": null } }),
                json!({ "maybe": { "a": 2, "b": "y" } }),
                json!({ "maybe": null }),
                json!({ "items": [{ "a": 2, "b": "y" }] }),
            ],
        );

        // What is wrong whatever the defaults are is rejected: types, and the items of a list,
        // which is replaced whole.
        let validator = validator_of(&registry);
        for wrong in [
            json!({ "limit": "x" }),
            json!({ "nested": { "a": "x" } }),
            json!({ "items": [{ "a": 2 }] }),
            json!({ "items": [{ "a": null, "b": "y" }] }),
        ] {
            assert!(!validator.is_valid(&document(&wrong)), "{wrong}");
            assert!(!compiler_accepts(&registry, &wrong), "{wrong}");
        }
        // A published schema cannot tell that an override lacks what no default fills in: the
        // compiler refuses this, the schema lets it through. That is the one direction allowed.
        let incomplete = json!({ "maybe": { "a": 2 } });
        assert!(!compiler_accepts(&registry, &incomplete));
        assert!(validator.is_valid(&document(&incomplete)));
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
