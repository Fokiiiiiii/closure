//! Creates a module crate from the templates in `templates/`.
//!
//! The crate is the diagnostics module in miniature, written with the SDK: a manifest, a command
//! with a permission and a setting, two catalogs and a test that runs it.

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use closure_core::module::ModuleId;

/// Module ids the host or the shared catalog already use.
const RESERVED: [&str; 2] = ["core", "system"];

/// Each file of the module: its path (which may hold `{{id}}`) and its template.
const TEMPLATES: [(&str, &str); 5] = [
    ("Cargo.toml", include_str!("../templates/Cargo.toml.tmpl")),
    ("src/lib.rs", include_str!("../templates/lib.rs.tmpl")),
    ("locales/en.ftl", include_str!("../templates/en.ftl.tmpl")),
    ("locales/ja.ftl", include_str!("../templates/ja.ftl.tmpl")),
    ("tests/{{id}}.rs", include_str!("../templates/test.rs.tmpl")),
];

/// The names a module's files are built from.
#[derive(Debug, PartialEq, Eq)]
pub struct Names {
    /// The module id, as the configuration names it: `warn`.
    pub id: String,
    /// The crate, in the workspace: `closure-mod-warn`.
    pub package: String,
    /// The library, as Rust names it: `closure_mod_warn`.
    pub library: String,
    /// The display name: `Warn`, or `My Tool` for `my-tool`.
    pub name: String,
}

pub fn names(id: &str) -> Result<Names> {
    ModuleId::new(id)?;
    if RESERVED.contains(&id) {
        bail!("`{id}` is reserved; choose another module id");
    }
    let name = id
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(capitalize)
        .collect::<Vec<_>>()
        .join(" ");
    Ok(Names {
        id: id.to_owned(),
        package: format!("closure-mod-{id}"),
        library: format!("closure_mod_{}", id.replace('-', "_")),
        name,
    })
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_ascii_uppercase().to_string() + chars.as_str()
    })
}

/// Every file of the module as `(path, content)`.
pub fn render(names: &Names) -> Vec<(String, String)> {
    TEMPLATES
        .iter()
        .map(|(path, template)| (fill(path, names), fill(template, names)))
        .collect()
}

fn fill(text: &str, names: &Names) -> String {
    text.replace("{{id}}", &names.id)
        .replace("{{package}}", &names.package)
        .replace("{{library}}", &names.library)
        .replace("{{name}}", &names.name)
}

/// The files as a listing: a header line per file, then its content.
pub fn listing(files: &[(String, String)]) -> String {
    let mut out = String::new();
    for (path, content) in files {
        let _ = writeln!(out, "=== {path} ===");
        out.push_str(content);
    }
    out
}

/// Formats the generated Rust files the way the workspace does. The template's line breaks depend
/// on the length of the id, so rustfmt has the last word; a missing rustfmt is reported, not fatal.
pub fn format_sources(paths: &[PathBuf]) -> Result<()> {
    let sources: Vec<&PathBuf> = paths
        .iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect();
    let status = std::process::Command::new("rustfmt")
        .args(["--edition", "2024"])
        .args(sources)
        .status()
        .context("cannot run rustfmt; run `cargo fmt` on the new crate")?;
    if !status.success() {
        bail!("rustfmt failed on the new crate; run `cargo fmt` on it");
    }
    Ok(())
}

/// Adds the crate to the workspace's dependencies, after the other `closure-mod-*` entries.
pub fn add_workspace_dependency(cargo_toml: &str, package: &str) -> Result<String> {
    let already = cargo_toml.lines().any(|line| {
        line.split('=')
            .next()
            .is_some_and(|key| key.trim() == package)
    });
    if already {
        bail!("`{package}` is already a workspace dependency");
    }
    let mut lines: Vec<&str> = cargo_toml.lines().collect();
    let anchor = lines
        .iter()
        .rposition(|line| line.starts_with("closure-mod-"))
        .context("the workspace has no closure-mod-* dependency to place the new one after")?;
    let entry = format!("{package} = {{ path = \"crates/{package}\" }}");
    lines.insert(anchor + 1, entry.as_str());
    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_crate_and_library_conventions() {
        let names = names("my-tool").unwrap();
        assert_eq!(names.package, "closure-mod-my-tool");
        assert_eq!(names.library, "closure_mod_my_tool");
        assert_eq!(names.name, "My Tool");
    }

    #[test]
    fn invalid_and_reserved_ids_are_refused() {
        for id in [
            "",
            "Warn",
            "1warn",
            "a.b",
            "core",
            "system",
            &"a".repeat(33),
        ] {
            assert!(names(id).is_err(), "{id} must be refused");
        }
    }

    #[test]
    fn every_placeholder_is_filled() {
        for (_, content) in render(&names("example").unwrap()) {
            assert!(!content.contains("{{"), "{content}");
        }
    }

    #[test]
    fn the_workspace_entry_goes_after_the_last_module_entry() {
        let before = "[workspace.dependencies]\nclosure-mod-a = { path = \"a\" }\nclosure-mod-b = { path = \"b\" }\nanyhow = \"1\"\n";
        let after = add_workspace_dependency(before, "closure-mod-c").unwrap();
        assert_eq!(
            after,
            "[workspace.dependencies]\nclosure-mod-a = { path = \"a\" }\nclosure-mod-b = { path = \"b\" }\nclosure-mod-c = { path = \"crates/closure-mod-c\" }\nanyhow = \"1\"\n"
        );
        assert!(add_workspace_dependency(&after, "closure-mod-a").is_err());
        assert!(add_workspace_dependency("[workspace]\n", "closure-mod-c").is_err());
    }

    #[test]
    fn the_template_text_is_the_golden_listing() {
        let files = render(&names("example").unwrap());
        assert_eq!(
            listing(&files),
            include_str!("../golden/module-example.txt"),
            "regenerate with: cargo run -p closure-xtask -- new-module example --dry-run > crates/closure-xtask/golden/module-example.txt"
        );
    }
}
