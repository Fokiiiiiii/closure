use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

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
    // Every module's schema becomes a schema resource of the document, named by its `$id`; two
    // resources of one name would be told apart by nothing.
    let mut resource_ids: BTreeMap<String, String> = BTreeMap::new();
    for module in registry.iter() {
        let manifest = module.manifest();
        let module_id = manifest.id.to_string();
        let refuse = |message: String| ConfigError::BadModuleSchema {
            module: module_id.clone(),
            message,
        };
        check_supported(&manifest.config_schema).map_err(refuse)?;
        let (config_schema, resource_id) =
            published_config_schema(&module_id, &manifest.config_schema);
        if let Some(other) = resource_ids.insert(resource_id.clone(), module_id.clone()) {
            return Err(refuse(format!(
                "`$id` {resource_id} is also the `$id` of module `{other}`"
            )));
        }
        let capabilities: Vec<&str> = manifest
            .capabilities
            .iter()
            .map(Capability::as_str)
            .collect();
        module_properties.insert(
            module_id.clone(),
            json!({
                "type": "object",
                "description": manifest.description,
                "additionalProperties": false,
                "properties": {
                    "enabled": or_null(json!({ "type": "boolean", "default": manifest.default_enabled })),
                    "capabilities": or_null(json!({
                        "type": "array",
                        "uniqueItems": true,
                        "items": { "enum": capabilities }
                    })),
                    "config": config_schema
                }
            }),
        );
    }

    if let Some(Value::Object(properties)) = schema.pointer_mut("/properties") {
        properties.insert(
            "language".into(),
            or_null(json!({
                "description": "Language of the bot's messages; `auto` follows the server's community locale.",
                "type": "string",
                "enum": languages,
                "default": "auto"
            })),
        );
        properties.insert(
            "modules".into(),
            or_null(json!({
                "description": "Per-module settings, keyed by module id.",
                "type": "object",
                "additionalProperties": false,
                "properties": module_properties
            })),
        );
    }
    Ok(schema)
}

/// `null` for an optional field means the same as leaving it out.
fn or_null(schema: Value) -> Value {
    let mut wrapper = Map::new();
    wrapper.insert(
        "anyOf".into(),
        Value::Array(vec![schema, json!({ "type": "null" })]),
    );
    Value::Object(wrapper)
}

/// A module's config schema as it is published inside the document, and the name (`$id`) of the
/// schema resource it is.
fn published_config_schema(module: &str, schema: &Value) -> (Value, String) {
    let mut config = schema.clone();
    let mut resource_id = format!("urn:closure:module:{module}");
    if let Value::Object(map) = &mut config {
        map.remove("$schema");
        // A reference scope of its own: `#/$defs/..` in a module's schema means that schema's own
        // definitions, not those of the document it is embedded in or of another module. A
        // schema that names itself keeps its name.
        match map.get("$id") {
            Some(Value::String(own)) => resource_id.clone_from(own),
            _ => {
                map.insert("$id".into(), json!(resource_id));
            }
        }
    }
    relax(&mut config);
    // `config: null` stands for "the defaults" as well.
    (or_null(config), resource_id)
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

/// Keywords whose value is one schema.
const SCHEMAS: [&str; 10] = [
    "items",
    "additionalProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contains",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
];

/// Keywords whose value is a list of schemas.
const SCHEMA_LISTS: [&str; 4] = ["allOf", "anyOf", "oneOf", "prefixItems"];

/// Keywords for references that cannot be followed by name, so the schema cannot be translated.
const UNSUPPORTED: [&str; 5] = [
    "$anchor",
    "$dynamicAnchor",
    "$dynamicRef",
    "$recursiveAnchor",
    "$recursiveRef",
];

/// Keywords that judge which keys an object has, how many, or what they are called, or the object
/// as a whole. A sparse override lacks keys the defaults fill in and has `null` keys the merge
/// drops, and weakening what sits under `not` makes it stricter, so these are dropped where
/// values are merged.
const DROPPED: [&str; 8] = [
    "required",
    "dependentRequired",
    "dependentSchemas",
    "minProperties",
    "maxProperties",
    "propertyNames",
    "unevaluatedProperties",
    "not",
];

/// How deep [`relax`] follows nesting and references. What is deeper allows anything, which keeps
/// the contract (it can only let more through).
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
    scan(schema, true)
}

/// Looks at the places where a schema can have keywords. What a keyword holds that is not a schema
/// (`const`, `enum`, `default`, annotations, keywords nobody here knows) is data and left alone.
fn scan(schema: &Value, top: bool) -> Result<(), String> {
    let Value::Object(map) = schema else {
        return Ok(());
    };
    for (keyword, inner) in map {
        let keyword = keyword.as_str();
        if UNSUPPORTED.contains(&keyword) {
            return Err(format!("`{keyword}` is not supported"));
        }
        if matches!(keyword, "$id" | "$schema") && !top {
            return Err(format!(
                "`{keyword}` is only supported at the top of a module's schema"
            ));
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
        if SCHEMA_MAPS.contains(&keyword) {
            if let Value::Object(named) = inner {
                for schema in named.values() {
                    scan(schema, false)?;
                }
            }
        } else if SCHEMAS.contains(&keyword) {
            scan(inner, false)?;
        } else if SCHEMA_LISTS.contains(&keyword)
            && let Value::Array(schemas) = inner
        {
            for schema in schemas {
                scan(schema, false)?;
            }
        }
    }
    Ok(())
}

/// Turns the schema of a configuration into the schema of an override of it.
///
/// Values are merged key by key into the defaults, `null` meaning "the default", and arrays are
/// replaced whole. So, wherever values are merged: a property may be `null`, keywords that look
/// at which keys exist, how many or what they are called are dropped, so are an object `const`
/// or `enum` (a sparse override is never equal to one), `oneOf` and `if`/`then`/`else` are
/// weakened to what any of their branches allows, and an unknown key may only be `null` (the
/// merge drops it). A property may be a type defined elsewhere in the schema (`$ref` to `$defs`);
/// a reference reached the way a property is gets a copy of the definition made the same way,
/// and the original stays as it is for the places that need it strict, such as the items of a
/// list.
fn relax(schema: &mut Value) {
    let mut copies = Copies::new(schema);
    copies.visit(schema, 0);
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

    fn visit(&mut self, schema: &mut Value, depth: usize) {
        if depth > MAX_DEPTH {
            if schema.is_object() {
                *schema = Value::Bool(true);
            }
            return;
        }
        let Value::Object(map) = schema else { return };
        for keyword in DROPPED {
            map.remove(keyword);
        }
        // A sparse object is never equal to a whole one.
        if matches!(map.get("const"), Some(Value::Object(_))) {
            map.remove("const");
        }
        if matches!(map.get("enum"), Some(Value::Array(options)) if options.iter().any(Value::is_object))
        {
            map.remove("enum");
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
                    self.visit(branch, depth + 1);
                }
            }
        }
    }

    /// Lets `null` stand in for the value of a property.
    fn nullable(&mut self, sub: &mut Value, depth: usize) {
        self.visit(sub, depth + 1);
        let inner = sub.take();
        *sub = or_null(inner);
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
            self.visit(&mut copy, depth + 1);
            self.made.insert((place, candidate.clone()), copy);
            candidate
        };
        map.insert(
            "$ref".into(),
            json!(definition_reference(place, &copy_name)),
        );
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

/// `(where, name)` of a `$ref` like `#/$defs/Name` that points into the schema itself. The
/// reference is a URI fragment holding a JSON pointer, so it is read as one: percent escapes
/// first, then the pointer's segments, then `~1` and `~0` in each. It must be exactly one name
/// below `$defs` or `definitions`.
fn local_definition(reference: &str) -> Option<(&'static str, String)> {
    let pointer = percent_decode(reference.strip_prefix('#')?)?;
    let mut segments = pointer.strip_prefix('/')?.split('/');
    let first = segments.next()?;
    let place = DEFINITIONS.into_iter().find(|place| *place == first)?;
    let name = segments.next()?;
    if name.is_empty() || segments.next().is_some() {
        return None;
    }
    Some((place, name.replace("~1", "/").replace("~0", "~")))
}

/// The `$ref` that points to the definition `name` kept in `place`; the inverse of
/// [`local_definition`].
fn definition_reference(place: &str, name: &str) -> String {
    let segment = name.replace('~', "~0").replace('/', "~1");
    format!("#/{place}/{}", percent_encode(&segment))
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'%' {
            decoded.push(u8::from_str_radix(text.get(at + 1..at + 3)?, 16).ok()?);
            at += 3;
        } else {
            decoded.push(bytes[at]);
            at += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn percent_encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail.
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
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
            schema["properties"]["language"]["anyOf"][0]["enum"],
            json!(["auto", "en", "ja"])
        );
        let warn = &schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"];
        let config = &warn["properties"]["config"]["anyOf"][0];
        assert_eq!(config["properties"]["limit"]["anyOf"][0]["type"], "integer");
        assert!(config.get("$schema").is_none());
        assert_eq!(
            warn["properties"]["capabilities"]["anyOf"][0]["items"]["enum"],
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
        let config = &schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"]["properties"]
            ["config"]["anyOf"][0];
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
    fn a_field_the_parser_lets_be_null_may_be_null_in_the_published_document() {
        let registry = registry();
        let validator = validator_of(&registry);
        let schema = document_schema(&registry).unwrap();
        // Every field of the document: if `field: null` parses, the schema has to take it.
        for field in schema["properties"].as_object().unwrap().keys() {
            if crate::yaml::parse_overrides(&format!("{field}: null\n")).is_ok() {
                assert!(
                    validator.is_valid(&json!({ field: null })),
                    "`{field}: null` parses but the schema rejects it"
                );
            }
        }
        // And the fields of a module.
        for field in ["enabled", "capabilities", "config"] {
            let yaml = format!("modules:\n  warn:\n    {field}: null\n");
            assert!(crate::yaml::parse_overrides(&yaml).is_ok(), "{field}");
            assert!(
                validator.is_valid(&json!({ "modules": { "warn": { field: null } } })),
                "`{field}: null` of a module parses but the schema rejects it"
            );
        }
        // The document itself knows nothing of a field it does not have.
        assert!(!validator.is_valid(&json!({ "nonsense": null })));
    }

    #[test]
    fn keywords_that_count_or_name_keys_do_not_see_keys_the_merge_drops() {
        // `ghost: null` is dropped by the merge, so the merged object is as it was by default.
        for (schema, default) in [
            (json!({ "type": "object", "maxProperties": 0 }), json!({})),
            (
                json!({ "type": "object", "propertyNames": { "pattern": "^allowed$" } }),
                json!({}),
            ),
            (
                json!({
                    "type": "object",
                    "properties": { "a": { "type": "integer" } },
                    "unevaluatedProperties": false
                }),
                json!({ "a": 1 }),
            ),
        ] {
            let registry = Arc::new(registry_with_defaults(schema, default));
            assert_never_rejects_what_the_compiler_accepts(&registry, &[json!({ "ghost": null })]);
        }
    }

    #[test]
    fn an_object_const_or_enum_does_not_hold_a_sparse_override_to_the_whole_object() {
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "properties": {
                    "pair": { "const": { "a": 1, "b": 2 } },
                    "choice": { "enum": [{ "a": 1, "b": 2 }, { "a": 3, "b": 4 }] },
                    "mode": { "enum": ["x", "y"] }
                }
            }),
            json!({ "pair": { "a": 1, "b": 2 }, "choice": { "a": 1, "b": 2 }, "mode": "x" }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[
                json!({ "pair": { "a": 1 } }),
                json!({ "choice": { "a": 1 } }),
                json!({ "choice": { "a": 3, "b": 4 } }),
                json!({ "mode": "y" }),
            ],
        );
        // A scalar enum is as strict as it was.
        assert!(!validator_of(&registry).is_valid(&document(&json!({ "mode": "z" }))));
    }

    #[test]
    fn definitions_with_names_that_need_escaping_are_followed() {
        let inner = json!({ "type": "object", "properties": { "n": { "type": "integer" } } });
        let registry = registry_with(json!({
            "type": "object",
            "properties": {
                "slash": { "$ref": "#/$defs/A~1B" },
                "wide": { "$ref": "#/$defs/%C3%BC%20x" },
                "tilde": { "$ref": "#/$defs/a~0b" },
                "percent": { "$ref": "#/$defs/100%25" }
            },
            "$defs": { "A/B": inner, "ü x": inner, "a~b": inner, "100%": inner }
        }));
        let validator = validator_of(&registry);
        for field in ["slash", "wide", "tilde", "percent"] {
            // `null` for `n` can only be accepted through the copy, so it was followed.
            assert!(
                validator.is_valid(&document(&json!({ field: { "n": null } }))),
                "{field}"
            );
            assert!(
                !validator.is_valid(&document(&json!({ field: { "n": "x" } }))),
                "{field}"
            );
        }
    }

    #[test]
    fn two_modules_may_not_name_their_schemas_alike() {
        let module = |id: &str| {
            let mut manifest = ModuleManifest::new(ModuleId::new(id).unwrap(), id, "0.1.0");
            manifest.config_schema = json!({ "$id": "urn:example:shared", "type": "object" });
            let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
            module
        };
        let registry = ModuleRegistry::new(vec![module("first"), module("second")]).unwrap();

        let Err(error) = document_schema(&registry) else {
            panic!("the document must not be built");
        };
        assert!(error.to_string().contains("urn:example:shared"), "{error}");
        let Err(error) = ConfigCompiler::new(Arc::new(registry)) else {
            panic!("the bot must not start");
        };
        assert!(error.to_string().contains("urn:example:shared"), "{error}");
    }

    #[test]
    fn a_nested_schema_resource_is_refused() {
        // Another `$id` or `$schema` inside the schema would be a schema resource of its own, with
        // definitions of its own, and a dialect of its own: nothing here translates that.
        for schema in [
            json!({ "properties": { "child": { "$id": "urn:example:child", "type": "object" } } }),
            json!({ "$defs": { "Child": { "$id": "urn:example:child" } } }),
            json!({ "properties": { "child": { "$schema": "https://json-schema.org/draft/2020-12/schema" } } }),
        ] {
            let error = check_supported(&schema).expect_err(&schema.to_string());
            assert!(error.contains("only supported at the top"), "{error}");
        }
        // At the top they are fine, and so are properties that are *called* that.
        check_supported(
            &json!({ "$id": "urn:example:config", "properties": { "$id": { "type": "string" } } }),
        )
        .unwrap();
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
        let config = &schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"]["properties"]
            ["config"]["anyOf"][0];
        assert_eq!(config["$id"], "urn:example:config");
        let validator = validator_of(&registry);
        assert!(validator.is_valid(&document(&json!({ "x": null }))));
        assert!(!validator.is_valid(&document(&json!({ "x": "text" }))));
    }

    #[test]
    fn what_lies_beyond_the_depth_limit_allows_anything() {
        // A chain of definitions longer than the limit, each requiring its `child`, ending in a
        // type that requires `v`. The override goes all the way down and leaves `v` out.
        let length = MAX_DEPTH + 40;
        let mut definitions = Map::new();
        let (mut default, mut over) = (json!({ "v": 1 }), json!({}));
        for n in (0..length).rev() {
            let (required, properties) = if n + 1 == length {
                (json!(["v"]), json!({ "v": { "type": "integer" } }))
            } else {
                (
                    json!(["child"]),
                    json!({ "child": { "$ref": format!("#/$defs/D{}", n + 1) } }),
                )
            };
            definitions.insert(
                format!("D{n}"),
                json!({ "type": "object", "required": required, "properties": properties }),
            );
            if n + 1 != length {
                default = json!({ "child": default });
                over = json!({ "child": over });
            }
        }
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "properties": { "root": { "$ref": "#/$defs/D0" } },
                "$defs": definitions
            }),
            json!({ "root": default }),
        ));
        assert_never_rejects_what_the_compiler_accepts(&registry, &[json!({ "root": over })]);
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
            // `%2F` is a `/` once the fragment is read, so this goes deeper than one name.
            (json!({ "$ref": "#/$defs/a%2Fb" }), "a%2Fb"),
            (json!({ "$ref": "#/$defs/%ZZ" }), "%ZZ"),
            (json!({ "$ref": "#/$defs/" }), "#/$defs/"),
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
            // Names the way schemars writes them when they hold `/`, `~`, spaces or non-ASCII.
            json!({ "properties": { "a": { "$ref": "#/$defs/A~1B" }, "b": { "$ref": "#/$defs/%C3%BC%20x" } } }),
            // What nobody here knows is an annotation, whatever it holds.
            json!({ "x-ui": { "$ref": "https://example.invalid/help", "$anchor": "a" } }),
            // A schema that names itself.
            json!({ "$id": "urn:example:config", "$schema": "https://json-schema.org/draft/2020-12/schema" }),
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
