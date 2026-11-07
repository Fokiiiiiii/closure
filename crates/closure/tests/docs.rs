//! The README names what the repository holds: a module added or a claim that stopped being true
//! is caught here and not by a reader.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "a test that cannot read the repository has nothing to check"
)]

use std::fs;
use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels under the workspace root")
}

fn read(name: &str) -> String {
    fs::read_to_string(root().join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

#[test]
fn both_readmes_name_every_module_of_the_repository() {
    let crates = fs::read_dir(root().join("crates")).unwrap();
    let modules: Vec<String> = crates
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter_map(|name| name.strip_prefix("closure-mod-").map(str::to_owned))
        .collect();
    assert!(!modules.is_empty());
    for readme in ["README.md", "README.ja.md"] {
        let text = read(readme);
        for module in &modules {
            assert!(
                text.contains(module.as_str()),
                "{readme} does not name {module}"
            );
        }
    }
}

#[test]
fn the_readmes_do_not_call_scripting_unimplemented() {
    assert!(root().join("crates/closure-script").is_dir());
    for readme in ["README.md", "README.ja.md"] {
        let text = read(readme);
        assert!(!text.contains("are not\n  implemented"), "{readme}");
        assert!(!text.contains("実装はまだです"), "{readme}");
    }
}
