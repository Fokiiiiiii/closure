//! The reference of the shipped modules, generated from their manifests and catalogs.
//!
//! The output depends only on those inputs, so the same build always writes the same bytes;
//! `docs --check` relies on that.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use closure_core::locale::{Args, Locale, Translator};
use closure_core::module::{ChoiceValue, ModuleManifest, OptionKind, OptionSpec, SubcommandSpec};
use serde_json::Value;

/// The file of each module's reference, keyed by file name, for what should exist on disk.
pub fn expected_files<'a>(
    manifests: impl IntoIterator<Item = &'a ModuleManifest>,
    translator: &dyn Translator,
) -> BTreeMap<String, String> {
    manifests
        .into_iter()
        .map(|manifest| {
            (
                format!("{}.md", manifest.id),
                render_module(manifest, translator),
            )
        })
        .collect()
}

/// Files that differ from what should exist: changed, missing, or stale ones.
pub fn differences(
    expected: &BTreeMap<String, String>,
    actual: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut found = Vec::new();
    for (name, content) in expected {
        match actual.get(name) {
            None => found.push(format!("missing: {name}")),
            Some(on_disk) if on_disk != content => found.push(format!("changed: {name}")),
            Some(_) => {}
        }
    }
    for name in actual.keys().filter(|name| !expected.contains_key(*name)) {
        found.push(format!("stale: {name}"));
    }
    found
}

pub fn render_module(manifest: &ModuleManifest, translator: &dyn Translator) -> String {
    let module = manifest.id.as_str();
    let mut out = String::new();
    let _ = writeln!(out, "# {} (`{module}`)\n", manifest.name);
    if !manifest.description.is_empty() {
        let _ = writeln!(out, "{}\n", manifest.description);
    }
    let _ = writeln!(out, "- Version: {}", manifest.version);
    let _ = writeln!(
        out,
        "- Enabled by default: {}\n",
        if manifest.default_enabled {
            "yes"
        } else {
            "no"
        }
    );
    push_capabilities(&mut out, manifest);
    push_permissions(&mut out, manifest);
    push_commands(&mut out, manifest, translator);
    push_configuration(&mut out, manifest);
    out
}

fn push_capabilities(out: &mut String, manifest: &ModuleManifest) {
    out.push_str("## Capabilities\n\n");
    if manifest.capabilities.is_empty() {
        out.push_str("None.\n\n");
        return;
    }
    for capability in manifest.capabilities.iter() {
        let _ = writeln!(out, "- `{}`", capability.as_str());
    }
    out.push('\n');
}

fn push_permissions(out: &mut String, manifest: &ModuleManifest) {
    out.push_str("## Permissions\n\n");
    if manifest.permissions.is_empty() {
        out.push_str("This module declares no permissions.\n\n");
        return;
    }
    out.push_str("| Key | Default level | Description |\n|---|---|---|\n");
    for permission in &manifest.permissions {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} |",
            permission.key.as_str(),
            permission.default_level.get(),
            cell(&permission.description)
        );
    }
    out.push('\n');
}

fn push_commands(out: &mut String, manifest: &ModuleManifest, translator: &dyn Translator) {
    out.push_str("## Commands\n\n");
    if manifest.commands.is_empty() {
        out.push_str("This module declares no commands.\n\n");
        return;
    }
    let module = manifest.id.as_str();
    for command in &manifest.commands {
        let _ = writeln!(out, "### `/{}`\n", command.name);
        let key = format!("command-{}", command.name);
        let english = default_text(translator, module, &format!("{key}.description"));
        push_catalog_names(out, translator, module, &key, &english);
        // The catalog path of a command is its name; subcommands extend it with their own.
        push_options(out, translator, module, &command.name, &command.options);
        if command.options.is_empty() && command.subcommands.is_empty() {
            out.push_str("This command takes no options.\n\n");
        }
        for subcommand in &command.subcommands {
            push_subcommand(
                out,
                translator,
                module,
                &command.name,
                &command.name,
                subcommand,
                4,
            );
        }
    }
}

/// A subcommand, or a group of them: `slash` is the command's path as users type it, `parent`
/// the catalog path of the command or group it belongs to.
fn push_subcommand(
    out: &mut String,
    translator: &dyn Translator,
    module: &str,
    slash: &str,
    parent: &str,
    spec: &SubcommandSpec,
    level: usize,
) {
    let slash = format!("{slash} {}", spec.name);
    let catalog_path = format!("{parent}-{}", spec.name);
    let _ = writeln!(out, "{} `/{slash}`\n", "#".repeat(level.min(6)));
    let key = format!("subcommand-{parent}-{}", spec.name);
    let english = default_text(translator, module, &format!("{key}.description"));
    push_catalog_names(out, translator, module, &key, &english);
    push_options(out, translator, module, &catalog_path, &spec.options);
    for subcommand in &spec.subcommands {
        push_subcommand(
            out,
            translator,
            module,
            &slash,
            &catalog_path,
            subcommand,
            level + 1,
        );
    }
}

/// The English description, then the Japanese name and description when the catalog has them.
fn push_catalog_names(
    out: &mut String,
    translator: &dyn Translator,
    module: &str,
    key: &str,
    description: &str,
) {
    let _ = writeln!(out, "{description}\n");
    if let Some(name) = japanese(translator, module, key) {
        let _ = writeln!(out, "- Japanese name: `{name}`");
    }
    if let Some(text) = japanese(translator, module, &format!("{key}.description")) {
        let _ = writeln!(out, "- Japanese description: {text}");
    }
    if !out.ends_with("\n\n") {
        out.push('\n');
    }
}

/// The text of a catalog entry in the default language; empty when the catalog has none.
fn default_text(translator: &dyn Translator, module: &str, key: &str) -> String {
    translator
        .exact(Locale::DEFAULT, module, key, &Args::new())
        .unwrap_or_default()
}

fn push_options(
    out: &mut String,
    translator: &dyn Translator,
    module: &str,
    prefix: &str,
    options: &[OptionSpec],
) {
    if options.is_empty() {
        return;
    }
    out.push_str("| Option | Japanese name | Type | Required | Limits | Description |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for option in options {
        let name = japanese(
            translator,
            module,
            &format!("option-{prefix}-{}", option.name),
        );
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {} | {} |",
            option.name,
            name.map_or_else(|| "-".to_owned(), |name| cell(&name)),
            kind_name(option.kind),
            if option.required { "yes" } else { "no" },
            limits(option),
            cell(&default_text(
                translator,
                module,
                &format!("option-{prefix}-{}.description", option.name),
            ))
        );
    }
    out.push('\n');
}

fn push_configuration(out: &mut String, manifest: &ModuleManifest) {
    out.push_str("## Configuration\n\n");
    let _ = writeln!(
        out,
        "Set under `modules.{}.config` in a server's configuration; see [configuration](../configuration.md).\n",
        manifest.id
    );
    let properties = manifest
        .config_schema
        .get("properties")
        .and_then(Value::as_object);
    match properties {
        Some(properties) if !properties.is_empty() => {
            out.push_str(
                "| Key | Type | Default | Widget | Description |\n|---|---|---|---|---|\n",
            );
            // Sorted, so the table does not depend on the order the manifest lists them in.
            let sorted: BTreeMap<&String, &Value> = properties.iter().collect();
            for (key, schema) in sorted {
                let default = manifest
                    .default_config
                    .get(key)
                    .or_else(|| schema.get("default"));
                let _ = writeln!(
                    out,
                    "| `{key}` | {} | {} | {} | {} |",
                    schema_type(schema),
                    default.map_or_else(|| "-".to_owned(), |value| format!("`{value}`")),
                    schema
                        .pointer("/x-ui/widget")
                        .and_then(Value::as_str)
                        .unwrap_or("-"),
                    schema
                        .get("description")
                        .and_then(Value::as_str)
                        .map_or_else(String::new, cell)
                );
            }
            out.push('\n');
        }
        _ => out.push_str("This module has no settings.\n\n"),
    }
    out.push_str("Default configuration:\n\n```json\n");
    let defaults =
        serde_json::to_string_pretty(&manifest.default_config).unwrap_or_else(|_| "{}".to_owned());
    out.push_str(&defaults);
    out.push_str("\n```\n");
}

fn japanese(translator: &dyn Translator, module: &str, key: &str) -> Option<String> {
    translator.exact(Locale::Ja, module, key, &Args::new())
}

fn kind_name(kind: OptionKind) -> &'static str {
    match kind {
        OptionKind::String => "string",
        OptionKind::Integer => "integer",
        OptionKind::Number => "number",
        OptionKind::Boolean => "boolean",
        OptionKind::User => "user",
        OptionKind::Channel => "channel",
        OptionKind::Role => "role",
        OptionKind::Mentionable => "mentionable",
        OptionKind::Attachment => "attachment",
    }
}

/// The constraints an option declares, in one cell.
fn limits(option: &OptionSpec) -> String {
    let mut parts = Vec::new();
    if !option.choices.is_empty() {
        let choices: Vec<String> = option
            .choices
            .iter()
            .map(|choice| format!("{} = {}", choice.name, choice_value(&choice.value)))
            .collect();
        parts.push(format!("choices: {}", choices.join(", ")));
    }
    if let Some(min) = option.min_value {
        parts.push(format!("min {min}"));
    }
    if let Some(max) = option.max_value {
        parts.push(format!("max {max}"));
    }
    if let Some(min) = option.min_length {
        parts.push(format!("min_length {min}"));
    }
    if let Some(max) = option.max_length {
        parts.push(format!("max_length {max}"));
    }
    if parts.is_empty() {
        "-".to_owned()
    } else {
        cell(&parts.join("; "))
    }
}

fn choice_value(value: &ChoiceValue) -> String {
    match value {
        ChoiceValue::String(text) => text.clone(),
        ChoiceValue::Integer(number) => number.to_string(),
        ChoiceValue::Number(number) => number.to_string(),
    }
}

/// The JSON type of a setting, as the reference states it.
fn schema_type(schema: &Value) -> String {
    match schema.get("type") {
        Some(Value::String(name)) => name.clone(),
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ if schema.get("enum").is_some() => "enum".to_owned(),
        _ => "any".to_owned(),
    }
}

/// Text for a Markdown table cell: a pipe would end the cell, a newline the row.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use closure_core::capability::Capability;
    use closure_core::locale::{KeyTranslator, Locale, Translator};
    use closure_core::module::{
        ChoiceValue, CommandSpec, ModuleId, OptionChoice, OptionKind, OptionSpec, SubcommandSpec,
    };
    use closure_core::permission::{Level, PermissionSpec};
    use closure_i18n::TranslatorBuilder;
    use serde_json::json;

    use super::*;

    const EN: &str = "
module-description = A sample | with a pipe.
command-sample = sample
    .description = Run the sample
option-sample-count = count
    .description = How many
option-sample-level = level
    .description = Level
option-sample-limit = limit
    .description = Most items.
command-tools = tools
    .description = Tools
subcommand-tools-add = add
    .description = Add one
option-tools-add-user = user
    .description = Who to add
subcommand-tools-lists = lists
    .description = Lists
subcommand-tools-lists-show = show
    .description = Show a list
";
    const JA: &str = "
command-sample = サンプル
    .description = サンプルを実行します
option-sample-count = 件数
option-sample-level = 水準
command-tools = tools
subcommand-tools-add = 追加
option-tools-add-user = ユーザー
";

    fn sample() -> ModuleManifest {
        let count = OptionSpec::new("count", OptionKind::Integer).required();
        let count = OptionSpec {
            min_value: Some(1.0),
            max_value: Some(10.0),
            ..count
        };
        let level = OptionSpec {
            choices: vec![OptionChoice {
                name: "low".into(),
                value: ChoiceValue::String("l".into()),
            }],
            ..OptionSpec::new("level", OptionKind::String)
        };
        let user = OptionSpec::new("user", OptionKind::User).required();
        ModuleManifest::new(ModuleId::new("sample").unwrap(), "Sample", "0.1.0")
            .description("A sample | with a pipe.")
            .capabilities([Capability::SendMessages])
            .permission(PermissionSpec::new("sample.use", Level::MODERATOR, "Use | it").unwrap())
            .config(
                json!({
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "Most items.",
                            "default": 5,
                            "x-ui": { "widget": "role" }
                        },
                        "mode": { "enum": ["a", "b"] }
                    }
                }),
                json!({ "limit": 3 }),
            )
            .command(CommandSpec {
                name: "sample".into(),
                options: vec![count, level],
                subcommands: Vec::new(),
                default_member_permissions: None,
            })
            .command(CommandSpec {
                name: "tools".into(),
                options: Vec::new(),
                subcommands: vec![
                    SubcommandSpec {
                        name: "add".into(),
                        options: vec![user],
                        subcommands: Vec::new(),
                    },
                    SubcommandSpec {
                        name: "lists".into(),
                        options: Vec::new(),
                        subcommands: vec![SubcommandSpec {
                            name: "show".into(),
                            options: Vec::new(),
                            subcommands: Vec::new(),
                        }],
                    },
                ],
                default_member_permissions: None,
            })
            .enabled_by_default()
    }

    fn japanese() -> impl Translator {
        let mut builder = TranslatorBuilder::new().unwrap();
        builder.add_catalog("sample", Locale::En, EN).unwrap();
        builder.add_catalog("sample", Locale::Ja, JA).unwrap();
        builder.build()
    }

    #[test]
    fn the_reference_shows_the_module_as_its_manifest_declares_it() {
        let text = render_module(&sample(), &japanese());
        for line in [
            "# Sample (`sample`)",
            "- Enabled by default: yes",
            "- `send_messages`",
            "| `sample.use` | 50 | Use \\| it |",
            "- Japanese name: `サンプル`",
            "- Japanese description: サンプルを実行します",
            "| `count` | 件数 | integer | yes | min 1; max 10 | How many |",
            "| `level` | 水準 | string | no | choices: low = l | Level |",
            "#### `/tools add`",
            "##### `/tools lists show`",
            "| `user` | ユーザー | user | yes | - | Who to add |",
            "| `limit` | integer | `3` | role | Most items. |",
            "| `mode` | enum | - | - |  |",
        ] {
            assert!(text.contains(line), "missing `{line}` in:\n{text}");
        }
    }

    #[test]
    fn the_same_manifest_always_renders_the_same_text() {
        let first = render_module(&sample(), &japanese());
        assert_eq!(first, render_module(&sample(), &japanese()));
        assert!(first.ends_with('\n'));
    }

    #[test]
    fn a_module_without_options_or_settings_says_so() {
        let bare = ModuleManifest::new(ModuleId::new("bare").unwrap(), "Bare", "0.1.0");
        let text = render_module(&bare, &KeyTranslator);
        assert!(
            text.contains("This module declares no permissions."),
            "{text}"
        );
        assert!(text.contains("This module has no settings."), "{text}");
    }

    #[test]
    fn identical_files_have_no_differences() {
        let files = BTreeMap::from([("a.md".to_owned(), "x".to_owned())]);
        assert!(differences(&files, &files).is_empty());
    }

    #[test]
    fn a_changed_missing_or_stale_file_is_reported() {
        let expected = BTreeMap::from([
            ("a.md".to_owned(), "new".to_owned()),
            ("b.md".to_owned(), "b".to_owned()),
        ]);
        let actual = BTreeMap::from([
            ("a.md".to_owned(), "old".to_owned()),
            ("c.md".to_owned(), "c".to_owned()),
        ]);
        assert_eq!(
            differences(&expected, &actual),
            ["changed: a.md", "missing: b.md", "stale: c.md"]
        );
    }
}
