use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use closure_core::capability::Capability;
use closure_core::locale::Locale;
use closure_core::registry::ModuleRegistry;
use serde_json::{Map, Value, json};

use crate::error::ConfigError;
use crate::model::GuildOverrides;
use crate::view::{self, BASIC_KEYS, View};

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
    document_schema_for(registry, View::Advanced)
}

/// [`document_schema`] for one view: the basic one keeps only the settings a simple form edits
/// (see [`view`]), and is still a schema of an override, so the same contract holds for it.
pub fn document_schema_for(registry: &ModuleRegistry, view: View) -> Result<Value, ConfigError> {
    let mut schema = serde_json::to_value(schemars::schema_for!(GuildOverrides))
        .map_err(|error| ConfigError::Serialize(error.to_string()))?;
    // The numbers of the document; what is an id (a string or a number) is not one of them.
    for numbers in ["/properties/version", "/$defs/LimitsOverride"] {
        if let Some(numbers) = schema.pointer_mut(numbers) {
            also_as_text(numbers);
        }
    }
    if let Some(preset) = schema.pointer_mut("/$defs/Preset") {
        also_as_unit_variant_map(preset);
    }
    // The parser takes `null` for a map as an empty one, and for the settings of a key as none.
    if let Some(settings) =
        schema.pointer_mut("/$defs/PermissionsOverride/properties/keys/additionalProperties")
    {
        *settings = or_null(settings.take());
    }
    for map in [
        "/$defs/PermissionsOverride/properties/roles",
        "/$defs/PermissionsOverride/properties/users",
        "/$defs/PermissionsOverride/properties/keys",
    ] {
        if let Some(map) = schema.pointer_mut(map) {
            *map = or_null(map.take());
        }
    }

    let mut languages = vec![json!("auto")];
    languages.extend(Locale::ALL.iter().map(|locale| json!(locale.tag())));

    let module_properties = module_entries(registry, view)?;

    if let Some(Value::Object(properties)) = schema.pointer_mut("/properties") {
        properties.insert(
            "language".into(),
            json!({
                "description": "Language of the bot's messages; `auto` follows the server's community locale.",
                "anyOf": [
                    { "type": "string", "enum": languages, "default": "auto" },
                    // What the parser also takes: any capitalisation, and a regional form.
                    { "type": "string", "pattern": language_alias_pattern() },
                    { "type": "null" }
                ]
            }),
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
    if view == View::Basic
        && let Some(Value::Object(properties)) = schema.pointer_mut("/properties")
    {
        properties.retain(|key, _| BASIC_KEYS.contains(&key.as_str()) || key == "modules");
    }
    // The whole document may be `null` too: an empty one.
    if let Value::Object(root) = &mut schema {
        root.insert("type".into(), json!(["object", "null"]));
    }
    Ok(schema)
}

/// The published entry of each module, keyed by id. The basic view has no capabilities in it, and
/// only the config a module has basic settings in.
fn module_entries(
    registry: &ModuleRegistry,
    view: View,
) -> Result<Map<String, Value>, ConfigError> {
    let mut module_properties = Map::new();
    for module in registry.iter() {
        let manifest = module.manifest();
        let module_id = manifest.id.to_string();
        let refuse = |message: String| ConfigError::BadModuleSchema {
            module: module_id.clone(),
            message,
        };
        check_supported(&manifest.config_schema).map_err(refuse)?;
        let mut properties = Map::new();
        // The parser also reads `"yes"` and the like as a boolean.
        properties.insert(
            "enabled".into(),
            or_null(json!({
                "type": ["boolean", "string"],
                "default": manifest.default_enabled
            })),
        );
        if view == View::Advanced {
            let capabilities: Vec<&str> = manifest
                .capabilities
                .iter()
                .map(Capability::as_str)
                .collect();
            // Repeats are taken as one capability, so they are no mistake.
            properties.insert(
                "capabilities".into(),
                or_null(json!({
                    "type": "array",
                    "items": unit_variant_forms(&json!({ "enum": capabilities }))
                })),
            );
        }
        let config = match view {
            View::Advanced => Some(published_config_schema(&module_id, &manifest.config_schema)),
            View::Basic => view::basic_schema(&manifest.config_schema, false)
                .map(|basic| published_config_schema(&module_id, &basic)),
        };
        if let Some(config) = config {
            properties.insert("config".into(), config);
        }
        module_properties.insert(
            module_id,
            // A module's entry may be `null` as well: it stands for no override at all.
            json!({
                "type": ["object", "null"],
                "description": manifest.description,
                "additionalProperties": false,
                "properties": properties
            }),
        );
    }
    Ok(module_properties)
}

/// Where the document wants a number or a boolean, the parser also takes the text of one (`"1"`,
/// `"0x10"`, `"yes"`), quoted or not, so the published schema does too.
fn also_as_text(schema: &mut Value) {
    let Value::Object(map) = schema else { return };
    match map.get_mut("type") {
        Some(Value::String(kind)) if matches!(kind.as_str(), "integer" | "boolean") => {
            let kind = kind.clone();
            map.insert("type".into(), json!([kind, "string"]));
        }
        Some(Value::Array(kinds))
            if kinds
                .iter()
                .any(|kind| matches!(kind.as_str(), Some("integer" | "boolean")))
                && !kinds.iter().any(|kind| kind == "string") =>
        {
            kinds.push(json!("string"));
        }
        _ => {}
    }
    for inner in subschemas_mut(map) {
        also_as_text(inner);
    }
}

/// A string `enum` of names also takes each name as the only key of a map holding `null`, which
/// is how the parser reads a variant without a value.
fn unit_variant_forms(names: &Value) -> Value {
    let keys: Map<String, Value> = names
        .get("enum")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| (name.to_owned(), json!({ "type": "null" })))
        .collect();
    json!({
        "anyOf": [
            names,
            { "type": "object", "properties": keys, "additionalProperties": false }
        ]
    })
}

/// [`unit_variant_forms`] for a schema that is the `enum` itself, keeping what is said about it.
fn also_as_unit_variant_map(schema: &mut Value) {
    let Value::Object(map) = schema else { return };
    let description = map.remove("description");
    let mut forms = unit_variant_forms(&schema.take());
    if let (Some(description), Value::Object(outer)) = (description, &mut forms) {
        outer.insert("description".into(), description);
    }
    *schema = forms;
}

/// The spellings of a language the parser takes besides the canonical ones: `auto` in any
/// capitalisation, and a locale tag in any capitalisation, alone or followed by `-` or `_` and
/// anything (`ja-JP`, `en_US`).
fn language_alias_pattern() -> String {
    let insensitive = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphabetic() {
                    format!("[{}{}]", c.to_ascii_lowercase(), c.to_ascii_uppercase())
                } else {
                    c.to_string()
                }
            })
            .collect()
    };
    let tags: Vec<String> = Locale::ALL
        .iter()
        .map(|locale| insensitive(locale.tag()))
        .collect();
    format!(
        "^({}|({})([-_][\\s\\S]*)?)$",
        insensitive("auto"),
        tags.join("|")
    )
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

/// A module's config schema as it is published inside the document.
fn published_config_schema(module: &str, schema: &Value) -> Value {
    let mut config = schema.clone();
    if let Value::Object(map) = &mut config {
        map.remove("$schema");
        // A reference scope of its own: `#/$defs/..` in a module's schema means that schema's own
        // definitions, not those of the document it is embedded in or of another module. Its name
        // is the bot's to give, not the module's: a schema that refers only to itself has no use
        // for one, and two modules (or one module and the document) with names that mean the
        // same URI would be told apart by nothing.
        map.insert("$id".into(), json!(format!("urn:closure:module:{module}")));
    }
    relax(&mut config);
    // `config: null` stands for "the defaults" as well.
    or_null(config)
}

const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Where a schema keeps definitions that `$ref` can point to.
const DEFINITIONS: [&str; 2] = ["$defs", "definitions"];

/// Keywords whose values are schemas, by name.
const SCHEMA_MAPS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];

/// Keywords whose value is one schema.
const SCHEMAS: [&str; 11] = [
    "items",
    "additionalProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contains",
    "contentSchema",
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
/// as a whole (`dependencies` is the older spelling of `dependentRequired` and
/// `dependentSchemas`). A sparse override lacks keys the defaults fill in and has `null` keys the
/// merge drops, and weakening what sits under `not` makes it stricter, so these are dropped where
/// values are merged. So are the `unevaluated*` keywords: they judge what no other keyword looked
/// at, and the relaxing takes away or changes what looked at it (`if`, a branch cut off at the
/// depth limit).
const DROPPED: [&str; 10] = [
    "required",
    "dependentRequired",
    "dependentSchemas",
    "dependencies",
    "minProperties",
    "maxProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "not",
];

/// How deep [`relax`] follows nesting and references. What is deeper allows anything, which keeps
/// the contract (it can only let more through, since the keywords that would be turned around by
/// it are dropped).
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
    scan(schema, true)?;
    check_levels(schema, false, "")
}

/// A setting inside a basic one is basic too: the basic view takes a basic node whole, so an
/// advanced marker below it could not be honoured.
fn check_levels(schema: &Value, inherited: bool, path: &str) -> Result<(), String> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(());
    };
    let basic = view::marked(schema).unwrap_or(inherited);
    for (name, child) in properties {
        let child_path = if path.is_empty() {
            name.clone()
        } else {
            format!("{path}.{name}")
        };
        if basic && view::marked(child) == Some(false) {
            return Err(format!(
                "`{child_path}` is marked advanced inside a basic setting"
            ));
        }
        check_levels(child, basic, &child_path)?;
    }
    Ok(())
}

/// The widgets a dashboard can draw for a setting.
const WIDGETS: [&str; 5] = ["role", "channel", "duration", "textarea", "user"];

/// Hints for the dashboard's forms. They are annotations the bot does not act on, so only their
/// shape is checked, and every other `x-` keyword is passed through as it is.
fn check_ui_hints(hints: &Value) -> Result<(), String> {
    let Value::Object(map) = hints else {
        return Err("`x-ui` must be an object".to_owned());
    };
    let text_within = |value: &Value, max: usize| {
        value
            .as_str()
            .is_some_and(|text| text.chars().count() <= max)
    };
    for (key, value) in map {
        let valid = match key.as_str() {
            "widget" => value
                .as_str()
                .is_some_and(|widget| WIDGETS.contains(&widget)),
            "group" => text_within(value, 40),
            "help" => text_within(value, 200),
            "order" => value.as_u64().is_some_and(|order| order <= 1000),
            // Which view a setting belongs to; see the `view` module.
            "level" => value
                .as_str()
                .is_some_and(|level| matches!(level, "basic" | "advanced")),
            _ => false,
        };
        if !valid {
            return Err(format!("`x-ui.{key}` is not a valid hint"));
        }
    }
    Ok(())
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
        if keyword == "x-ui" {
            check_ui_hints(inner)?;
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
/// list. The same goes for `$ref: "#"`, the schema itself: reached the way a property is, it is
/// the relaxed whole; reached anywhere else, it is a strict copy of the whole.
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
    /// What `$ref: "#"` is turned into while the schema is relaxed: a reference to the strict
    /// copy of the whole schema, which stays where the places that need it strict are, and goes
    /// back to `#` where the relaxed schema is wanted.
    root_reference: String,
}

impl Copies {
    fn new(root: &mut Value) -> Self {
        let mut taken = BTreeSet::new();
        for place in DEFINITIONS {
            if let Some(Value::Object(definitions)) = root.get(place) {
                taken.extend(definitions.keys().map(|name| (place, name.clone())));
            }
        }
        let mut root_name = String::from("Root__strict");
        while taken.contains(&("$defs", root_name.clone())) {
            root_name.push('_');
        }
        let root_reference = definition_reference("$defs", &root_name);
        let mut made = BTreeMap::new();
        if point_root_references_at(root, &root_reference)
            && let Value::Object(whole) = root
        {
            let mut strict = whole.clone();
            // What is left is the schema without its definitions and its identity; the definitions
            // are found where they are, in the root.
            for keyword in ["$defs", "definitions", "$id", "$schema"] {
                strict.remove(keyword);
            }
            taken.insert(("$defs", root_name.clone()));
            made.insert(("$defs", root_name), Value::Object(strict));
        }
        let mut originals = BTreeMap::new();
        for place in DEFINITIONS {
            if let Some(Value::Object(definitions)) = root.get(place) {
                for (name, definition) in definitions {
                    originals.insert((place, name.clone()), definition.clone());
                }
            }
        }
        Self {
            taken,
            originals,
            names: BTreeMap::new(),
            made,
            root_reference,
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
        if *target == self.root_reference {
            map.insert("$ref".into(), json!("#"));
            return;
        }
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
        let Value::Object(root) = root else { return };
        for ((place, name), copy) in self.made {
            if let Value::Object(definitions) = root.entry(place).or_insert_with(|| json!({})) {
                definitions.insert(name, copy);
            }
        }
    }
}

/// The schemas that sit directly in the keywords of `map`.
fn subschemas_mut(map: &mut Map<String, Value>) -> Vec<&mut Value> {
    let mut found = Vec::new();
    for (keyword, inner) in map.iter_mut() {
        let keyword = keyword.as_str();
        if SCHEMA_MAPS.contains(&keyword) {
            if let Value::Object(named) = inner {
                found.extend(named.values_mut());
            }
        } else if SCHEMAS.contains(&keyword) {
            found.push(inner);
        } else if SCHEMA_LISTS.contains(&keyword)
            && let Value::Array(schemas) = inner
        {
            found.extend(schemas.iter_mut());
        }
    }
    found
}

/// Points every `$ref: "#"` in the schema at `target`; whether there was one.
fn point_root_references_at(schema: &mut Value, target: &str) -> bool {
    let Value::Object(map) = schema else {
        return false;
    };
    let mut found = false;
    if map.get("$ref").and_then(Value::as_str) == Some("#") {
        map.insert("$ref".into(), json!(target));
        found = true;
    }
    for inner in subschemas_mut(map) {
        found |= point_root_references_at(inner, target);
    }
    found
}

/// Weakens `oneOf` and `if`/`then`/`else` to "any of the branches", as `allOf` entries, because a
/// sparse override cannot say which branch applies once the defaults are filled in. `if` and a
/// `then` or `else` without its partner say nothing the override can be held to, and are dropped;
/// without an `if`, `then` and `else` are not looked at at all.
fn fold_conditionals(map: &mut Map<String, Value>) {
    let mut alternatives = Vec::new();
    if let Some(Value::Array(branches)) = map.remove("oneOf") {
        alternatives.push(json!({ "anyOf": branches }));
    }
    let conditional = map.remove("if").is_some();
    let (then, otherwise) = (map.remove("then"), map.remove("else"));
    if let (true, Some(then), Some(otherwise)) = (conditional, then, otherwise) {
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
            // `from_str_radix` alone would take a sign for a digit.
            let digits = text.get(at + 1..at + 3)?;
            if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            decoded.push(u8::from_str_radix(digits, 16).ok()?);
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
    use closure_core::permission::{Level, PermissionSpec};
    use closure_core::testing::StubModule;

    use super::*;
    use crate::compile::ConfigCompiler;
    use crate::model::{GuildOverrides, ModuleOverride, Preset};
    use crate::yaml::{overrides_to_yaml, parse_overrides};

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
        manifest.permissions =
            vec![PermissionSpec::new("warn.add", Level::MODERATOR, "add warnings").unwrap()];
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
            warn["properties"]["capabilities"]["anyOf"][0]["items"]["anyOf"][0]["enum"],
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
            json!({ "x-note": { "$ref": "https://example.invalid/help", "$anchor": "a" } }),
            // A schema that names itself.
            json!({ "$id": "urn:example:config", "$schema": "https://json-schema.org/draft/2020-12/schema" }),
            // A property is not a keyword, whatever it is called.
            json!({ "properties": { "$anchor": { "type": "string" }, "$ref": { "type": "string" } } }),
        ] {
            check_supported(&schema).unwrap_or_else(|error| panic!("{schema}: {error}"));
        }
    }

    /// Whether some object in `value` has `key` set to exactly `wanted`.
    fn holds_member(value: &Value, key: &str, wanted: &Value) -> bool {
        match value {
            Value::Object(map) => {
                map.get(key) == Some(wanted)
                    || map.values().any(|inner| holds_member(inner, key, wanted))
            }
            Value::Array(items) => items.iter().any(|inner| holds_member(inner, key, wanted)),
            _ => false,
        }
    }

    #[test]
    fn ui_hints_are_accepted_and_kept_in_the_published_schema() {
        let hint = json!({ "widget": "role", "group": "Roles", "order": 3, "help": "Who may act" });
        let schema = json!({
            "type": "object",
            "properties": {
                "role": { "type": "integer", "x-ui": hint },
                // Other annotations with the `x-` prefix are passed through as they are.
                "note": { "type": "string", "x-note": { "anything": [1, 2] } }
            }
        });
        check_supported(&schema).unwrap();
        for widget in ["role", "channel", "duration", "textarea", "user"] {
            check_supported(&json!({ "x-ui": { "widget": widget } })).unwrap();
        }
        check_supported(&json!({ "x-ui": { "order": 0 } })).unwrap();
        check_supported(&json!({ "x-ui": { "order": 1000, "group": "g".repeat(40) } })).unwrap();

        let registry = registry_with(schema);
        let published = document_schema(&registry).unwrap();
        assert!(holds_member(&published, "x-ui", &hint), "{published}");
        assert!(
            holds_member(&published, "x-note", &json!({ "anything": [1, 2] })),
            "{published}"
        );
    }

    #[test]
    fn ui_hints_of_the_wrong_shape_are_refused() {
        for hint in [
            json!("role"),
            json!({ "widget": "dropdown" }),
            json!({ "widget": 1 }),
            json!({ "color": "red" }),
            json!({ "group": "g".repeat(41) }),
            json!({ "help": "h".repeat(201) }),
            json!({ "order": -1 }),
            json!({ "order": 1001 }),
            json!({ "order": 1.5 }),
            json!({ "order": "1" }),
        ] {
            let schema = json!({ "properties": { "a": { "x-ui": hint } } });
            let error = check_supported(&schema).expect_err(&schema.to_string());
            assert!(error.contains("x-ui"), "{error}");
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
    fn schema_publishes_the_guild_wide_settings() {
        let schema = document_schema(&registry()).unwrap();
        for key in ["timezone", "prefix", "success_emoji", "error_emoji"] {
            assert!(schema["properties"][key].is_object(), "{key}");
        }
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.is_valid(&json!({ "timezone": "Asia/Tokyo", "prefix": "!" })));
        assert!(validator.is_valid(&json!({ "timezone": null, "error_emoji": "" })));
        assert!(!validator.is_valid(&json!({ "prefix": "a b" })));
        assert!(!validator.is_valid(&json!({ "prefix": "" })));
    }

    #[test]
    fn schema_rejects_what_the_parser_rejects() {
        let schema = document_schema(&registry()).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(!validator.is_valid(&json!({ "language": "klingon" })));
        assert!(!validator.is_valid(&json!({ "modules": { "nope": {} } })));
        assert!(validator.is_valid(&json!({ "language": "ja" })));
    }

    /// Every document the parser takes and the compiler accepts, in the odd forms it takes them,
    /// must satisfy the published schema as well.
    #[test]
    fn documents_the_parser_accepts_satisfy_the_published_schema() {
        let registry = Arc::new(registry());
        let validator = validator_of(&registry);
        for document in [
            Value::Null,
            json!({ "modules": { "warn": null } }),
            json!({ "language": "AUTO" }),
            json!({ "language": "ja-JP" }),
            json!({ "language": "en_US" }),
            json!({ "language": "EN" }),
            json!({ "log_channel": 123_456_789_012_345_678_u64 }),
            json!({ "log_channel": "123456789012345678" }),
            json!({ "moderator_roles": [1, 2] }),
            json!({ "moderator_roles": ["1", 2] }),
            json!({ "modules": { "warn": { "capabilities": ["send_messages", "send_messages"] } } }),
            // The parser reads the text of a number or a boolean wherever one is wanted, quoted
            // or not, and a unit variant as a map holding it.
            json!({ "version": "1" }),
            json!({ "limits": { "concurrency": "2" } }),
            json!({ "limits": { "handler_timeout_secs": "0x10", "queue_capacity": "2_56" } }),
            json!({ "modules": { "warn": { "enabled": "yes" } } }),
            json!({ "modules": { "warn": { "enabled": " false " } } }),
            json!({ "preset": { "small": null } }),
            json!({ "modules": { "warn": { "capabilities": [{ "send_messages": null }] } } }),
            // The permissions section, in the forms the parser reads.
            json!({ "permissions": null }),
            json!({ "permissions": {} }),
            json!({ "permissions": { "administrator_level": "admin" } }),
            json!({ "permissions": { "administrator_level": 60, "roles": { "1": "moderator", "2": 30 } } }),
            json!({ "permissions": { "users": { "3": "owner" }, "roles": null } }),
            json!({ "permissions": { "users": null, "keys": null } }),
            json!({ "permissions": { "keys": { "warn.add": null } } }),
            json!({ "permissions": { "keys": { "warn.add": {
                "min_level": "trusted",
                "allow": { "roles": [1, "2"], "users": [3] },
                "deny": { "users": ["9"] }
            } } } }),
            json!({ "permissions": { "keys": { "warn.add": { "min_level": null, "allow": null, "deny": { "roles": null } } } } }),
            json!({ "moderator_roles": [5], "permissions": { "roles": { "5": 10 } } }),
        ] {
            let overrides = parse_overrides(&document.to_string()).unwrap_or_else(|error| {
                panic!("test setup: the parser refuses {document}: {error}")
            });
            ConfigCompiler::new(Arc::clone(&registry))
                .unwrap()
                .compile(&overrides)
                .unwrap_or_else(|error| {
                    panic!("test setup: the compiler refuses {document}: {error}")
                });
            assert!(
                validator.is_valid(&document),
                "the published schema rejects what the parser and compiler accept: {document}"
            );
        }
        for wrong in [
            json!({ "language": "klingon" }),
            json!({ "language": "english" }),
            json!({ "language": "enx" }),
            json!({ "log_channel": -1 }),
            json!({ "log_channel": "abc" }),
            json!({ "moderator_roles": ["abc"] }),
            json!({ "permissions": 5 }),
            json!({ "permissions": { "keys": 5 } }),
            json!({ "permissions": { "roles": { "1": true } } }),
            json!({ "permissions": { "unheard": 1 } }),
        ] {
            assert!(!validator.is_valid(&wrong), "{wrong}");
        }
    }

    #[test]
    fn what_a_dropped_keyword_evaluated_does_not_make_unevaluated_items_stricter() {
        // `if` passes for a first string, which then counts as evaluated; once `if` is folded away
        // nothing evaluates it, and `unevaluatedItems` would turn on the very item it allowed.
        let registry = Arc::new(registry_with(json!({
            "type": "object",
            "properties": { "list": {
                "type": "array",
                "if": { "prefixItems": [{ "type": "string" }] },
                "then": true,
                "else": true,
                "unevaluatedItems": false
            } }
        })));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[json!({ "list": ["x"] }), json!({ "list": [] })],
        );
    }

    #[test]
    fn then_and_else_without_if_say_nothing() {
        let registry = Arc::new(registry_with(json!({
            "type": "object",
            "properties": { "n": { "type": "integer" } },
            "then": false,
            "else": false
        })));
        assert_never_rejects_what_the_compiler_accepts(&registry, &[json!({}), json!({ "n": 1 })]);
    }

    #[test]
    fn a_reference_to_the_root_from_a_place_that_stays_strict_stays_strict() {
        // The items of a list are replaced whole, so they meet the root as the module wrote it:
        // `{}` lacks the required `n`. Pointing these at the relaxed root, which lets `{}` through,
        // would turn `not` and `maxContains` around and make `oneOf` match twice.
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "required": ["n"],
                "properties": {
                    "n": { "type": "integer" },
                    "negated": { "type": "array", "items": { "not": { "$ref": "#" } } },
                    "single": {
                        "type": "array",
                        "items": { "oneOf": [{ "$ref": "#" }, { "type": "object" }] }
                    },
                    "counted": {
                        "type": "array",
                        "contains": { "$ref": "#" },
                        "minContains": 0,
                        "maxContains": 0
                    }
                }
            }),
            json!({ "n": 1 }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[
                json!({ "negated": [{}] }),
                json!({ "single": [{}] }),
                json!({ "counted": [{}] }),
                json!({ "n": null }),
            ],
        );
        // Inside an item the root is still the whole, strict one.
        let validator = validator_of(&registry);
        assert!(!validator.is_valid(&document(&json!({ "negated": [{ "n": 1 }] }))));
    }

    #[test]
    fn the_legacy_dependencies_keyword_is_dropped_like_its_successors() {
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "properties": { "a": { "type": "integer" }, "b": { "type": "integer" } },
                "dependencies": { "a": ["b"], "b": { "required": ["a"] } }
            }),
            json!({ "a": 1, "b": 2 }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[json!({ "a": 3 }), json!({ "b": 4 })],
        );
        let schema = document_schema(&registry).unwrap();
        let config = &schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"]["properties"]
            ["config"]["anyOf"][0];
        assert!(config.get("dependencies").is_none());
    }

    #[test]
    fn a_reference_inside_content_schema_is_looked_at_too() {
        let error = check_supported(&json!({
            "type": "string",
            "contentMediaType": "application/json",
            "contentSchema": { "$ref": "urn:example:other" }
        }))
        .expect_err("a reference nobody can follow");
        assert!(error.contains("urn:example:other"), "{error}");
    }

    #[test]
    fn a_percent_escape_needs_two_hex_digits() {
        // `from_str_radix` alone would read `+1` as 1.
        assert_eq!(percent_decode("%2B1").as_deref(), Some("+1"));
        assert!(percent_decode("%+1").is_none());
        assert!(percent_decode("%-1").is_none());
        assert!(percent_decode("%1").is_none());
        assert!(local_definition("#/$defs/%+1").is_none());
    }

    #[test]
    fn a_modules_own_id_is_replaced_by_one_the_bot_gives_it() {
        // Two modules naming their schemas alike, or one naming itself like the other's assigned
        // name, in any spelling of the same URI, would be told apart by nothing. The `$id` has no
        // use for a schema that may only refer to itself, so the bot assigns it.
        // Each module's `X` is of a type of its own, so a reference that went to another module's
        // would be seen.
        let module = |id: &str, own: &str, kind: &str| {
            let mut manifest = ModuleManifest::new(ModuleId::new(id).unwrap(), id, "0.1.0");
            manifest.config_schema = json!({
                "$id": own,
                "type": "object",
                "properties": { "x": { "$ref": "#/$defs/X" } },
                "$defs": { "X": { "type": kind } }
            });
            let module: Arc<dyn Module> = Arc::new(StubModule(manifest));
            module
        };
        let registry = ModuleRegistry::new(vec![
            module("first", "urn:example:shared", "integer"),
            module("second", "URN:example:shared", "string"),
            module("third", "urn:closure:module:first", "boolean"),
            module("fourth", "#", "integer"),
            module("fifth", "", "string"),
        ])
        .unwrap();
        let schema = document_schema(&registry).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let modules = &schema["properties"]["modules"]["anyOf"][0]["properties"];
        for (id, right, wrong) in [
            ("first", json!(5), json!("t")),
            ("second", json!("t"), json!(5)),
            ("third", json!(true), json!(5)),
            ("fourth", json!(5), json!("t")),
            ("fifth", json!("t"), json!(5)),
        ] {
            assert_eq!(
                modules[id]["properties"]["config"]["anyOf"][0]["$id"],
                format!("urn:closure:module:{id}")
            );
            for (x, valid) in [(Value::Null, true), (right, true), (wrong, false)] {
                let document = json!({ "modules": { id: { "config": { "x": x } } } });
                assert_eq!(validator.is_valid(&document), valid, "{document}");
            }
        }
    }

    #[test]
    fn a_schema_that_is_not_an_object_gets_no_id() {
        let registry = registry_with(json!(true));
        let schema = document_schema(&registry).unwrap();
        let config = &schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"]["properties"]
            ["config"]["anyOf"][0];
        assert_eq!(config, &json!(true));
        let validator = validator_of(&registry);
        assert!(validator.is_valid(&document(&json!({ "anything": [1] }))));
    }

    #[test]
    fn a_reference_to_the_root_from_where_values_are_merged_is_the_relaxed_one() {
        // `child` meets the root as a merged value does, so what the defaults fill in may be
        // left out, down to the `null` that stands for "the default": the strict copy of the
        // root, which requires `n`, would refuse both.
        let registry = Arc::new(registry_with_defaults(
            json!({
                "type": "object",
                "required": ["n"],
                "properties": { "n": { "type": "integer" }, "child": { "$ref": "#" } }
            }),
            json!({ "n": 1, "child": { "n": 1 } }),
        ));
        assert_never_rejects_what_the_compiler_accepts(
            &registry,
            &[json!({ "child": {} }), json!({ "child": { "n": null } })],
        );
    }

    #[test]
    fn beyond_the_depth_limit_even_a_wrong_type_is_let_through() {
        // The compiler refuses the text at the bottom, which the schema cannot see through that
        // many levels of definitions; letting it through is the allowed direction, and it is
        // what tells the limit from the ordinary relaxing, which keeps the type.
        let length = MAX_DEPTH + 40;
        let mut definitions = Map::new();
        let (mut default, mut over) = (json!({ "v": 1 }), json!({ "v": "text" }));
        for n in (0..length).rev() {
            let properties = if n + 1 == length {
                json!({ "v": { "type": "integer" } })
            } else {
                json!({ "child": { "$ref": format!("#/$defs/D{}", n + 1) } })
            };
            definitions.insert(
                format!("D{n}"),
                json!({ "type": "object", "properties": properties }),
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
        let config = json!({ "root": over });
        assert!(!compiler_accepts(&registry, &config));
        assert!(validator_of(&registry).is_valid(&document(&config)));
    }

    /// A module whose basic settings are a top-level key and a nested one under an advanced object.
    fn layered_registry() -> ModuleRegistry {
        registry_with(json!({
            "type": "object",
            "properties": {
                "limit": { "type": "integer", "x-ui": { "level": "basic" } },
                "advanced": { "type": "integer" },
                "outer": {
                    "type": "object",
                    "required": ["inner", "other"],
                    "properties": {
                        "inner": { "type": "integer", "x-ui": { "level": "basic" } },
                        "other": { "type": "integer" }
                    }
                }
            },
            "required": ["limit", "advanced"],
            "additionalProperties": false
        }))
    }

    fn basic_document_schema(registry: &ModuleRegistry) -> Value {
        document_schema_for(registry, View::Basic).unwrap()
    }

    #[test]
    fn the_basic_schema_keeps_the_guild_wide_basic_keys_and_module_switches_only() {
        let schema = basic_document_schema(&layered_registry());
        let properties = schema["properties"].as_object().unwrap();
        for key in [
            "version",
            "preset",
            "language",
            "log_channel",
            "moderator_roles",
            "timezone",
            "prefix",
            "success_emoji",
            "error_emoji",
            "modules",
        ] {
            assert!(properties.contains_key(key), "{key}");
        }
        for key in ["limits", "permissions"] {
            assert!(!properties.contains_key(key), "{key}");
        }
        let warn = &schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"]["properties"];
        assert!(warn["enabled"].is_object(), "{warn}");
        assert!(warn.get("capabilities").is_none(), "{warn}");
    }

    #[test]
    fn the_basic_schema_prunes_module_settings_and_keeps_their_parents() {
        let schema = basic_document_schema(&layered_registry());
        let config = &schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"]["properties"]
            ["config"]["anyOf"][0];
        let properties = config["properties"].as_object().unwrap();
        assert!(properties.contains_key("limit"), "{config}");
        assert!(properties.contains_key("outer"), "{config}");
        assert!(!properties.contains_key("advanced"), "{config}");
        // Each property is published as `anyOf` with `null`, so the object is one level down.
        let outer = &config["properties"]["outer"]["anyOf"][0];
        assert!(outer["properties"]["inner"].is_object(), "{outer}");
        assert!(outer["properties"].get("other").is_none(), "{outer}");
    }

    #[test]
    fn a_module_without_basic_settings_has_no_config_in_the_basic_schema() {
        let registry = registry_with(json!({
            "type": "object",
            "properties": { "limit": { "type": "integer" } }
        }));
        let schema = basic_document_schema(&registry);
        let warn = &schema["properties"]["modules"]["anyOf"][0]["properties"]["warn"]["properties"];
        assert!(warn.get("config").is_none(), "{warn}");
        assert!(warn["enabled"].is_object());
    }

    #[test]
    fn the_basic_schema_accepts_a_basic_document_and_refuses_an_advanced_key() {
        let registry = layered_registry();
        let validator = jsonschema::validator_for(&basic_document_schema(&registry)).unwrap();
        let basic = json!({
            "language": "ja",
            "modules": { "warn": { "enabled": true, "config": { "limit": 3, "outer": { "inner": 1 } } } }
        });
        assert!(validator.is_valid(&basic), "{basic}");
        assert!(!validator.is_valid(&json!({ "limits": { "concurrency": 2 } })));
        assert!(!validator.is_valid(&json!({
            "modules": { "warn": { "config": { "advanced": 1 } } }
        })));
        assert!(!validator.is_valid(&json!({
            "modules": { "warn": { "capabilities": ["send_messages"] } }
        })));
    }

    #[test]
    fn the_advanced_view_is_the_published_schema_as_before() {
        let registry = layered_registry();
        assert_eq!(
            document_schema_for(&registry, View::Advanced).unwrap(),
            document_schema(&registry).unwrap()
        );
    }

    #[test]
    fn a_marker_must_name_a_level_and_not_undo_a_basic_parent() {
        for hint in [json!({ "level": "simple" }), json!({ "level": 1 })] {
            let schema = json!({ "properties": { "a": { "x-ui": hint } } });
            let error = check_supported(&schema).expect_err(&schema.to_string());
            assert!(error.contains("x-ui"), "{error}");
        }
        let schema = json!({
            "properties": {
                "a": {
                    "type": "object",
                    "x-ui": { "level": "basic" },
                    "properties": { "b": { "x-ui": { "level": "advanced" } } }
                }
            }
        });
        let error = check_supported(&schema).expect_err("an advanced key below a basic one");
        assert!(error.contains("a.b"), "{error}");
        check_supported(&json!({
            "properties": {
                "a": { "type": "object", "x-ui": { "level": "advanced" },
                       "properties": { "b": { "x-ui": { "level": "basic" } } } }
            }
        }))
        .unwrap();
    }
}
