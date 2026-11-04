use std::path::{Path, PathBuf};
use std::sync::Arc;

use closure_core::module::{Module, ModuleId, ModuleManifest, ModuleRuntime};
use serde_json::{Value, json};

use crate::runtime::{FakeRuntime, ScriptRuntime};
use crate::{Grants, RuntimeFactory, load_dir, load_grants};

struct Factory;
impl RuntimeFactory for Factory {
    fn create(&self, _: ModuleRuntime, _: &[u8]) -> Result<Option<Arc<dyn ScriptRuntime>>, String> {
        Ok(Some(Arc::new(FakeRuntime::new(|_, _, _| Ok(json!([]))))))
    }
}

struct NoRuntime;
impl RuntimeFactory for NoRuntime {
    fn create(&self, _: ModuleRuntime, _: &[u8]) -> Result<Option<Arc<dyn ScriptRuntime>>, String> {
        Ok(None)
    }
}

fn root(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("closure-loader-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn spec(id: &str) -> Value {
    let mut manifest = ModuleManifest::new(ModuleId::new(id).unwrap(), id, "1");
    manifest.runtime = ModuleRuntime::Wasm;
    let mut value = serde_json::to_value(manifest).unwrap();
    value["entry"] = json!("main.wasm");
    value
}

fn write(dir: &Path, name: &str, manifest: &Value) {
    let path = dir.join(name);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("main.wasm"), b"entry").unwrap();
    std::fs::write(
        path.join("module.yaml"),
        serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn loader_isolates_errors_and_sorts_modules() {
    let dir = root("ordered");
    write(&dir, "z", &spec("z"));
    write(&dir, "a", &spec("a"));
    write(&dir, "broken", &json!({"id":"broken"}));
    let loaded = load_dir(&dir, &Grants::new(), &Factory);
    assert_eq!(
        loaded
            .modules
            .iter()
            .map(|m| m.manifest().id.as_str())
            .collect::<Vec<_>>(),
        ["a", "z"]
    );
    assert_eq!(loaded.errors.len(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn loader_rejects_invalid_manifests_and_entries() {
    let dir = root("invalid");
    let mut cases = Vec::new();
    let mut unknown = spec("unknown");
    unknown["unexpected"] = json!(1);
    cases.push(("unknown", unknown));
    let mut native = spec("native");
    native["runtime"] = json!("native");
    cases.push(("native", native));
    cases.push(("wrong", spec("other")));
    let mut escape = spec("escape");
    escape["entry"] = json!("../outside");
    cases.push(("escape", escape));
    let mut absolute = spec("absolute");
    absolute["entry"] = json!("/tmp/outside");
    cases.push(("absolute", absolute));
    for (name, manifest) in &cases {
        write(&dir, name, manifest);
    }
    let loaded = load_dir(&dir, &Grants::new(), &Factory);
    assert_eq!(loaded.errors.len(), cases.len(), "{:?}", loaded.errors);
    assert!(loaded.modules.is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn duplicates_missing_directory_and_grants() {
    let dir = root("duplicates");
    assert!(
        load_dir(&dir.join("missing"), &Grants::new(), &Factory)
            .errors
            .is_empty()
    );
    assert!(load_grants(&dir).unwrap().is_empty());
    std::fs::write(dir.join("grants.yaml"), "same: [storage]\n").unwrap();
    assert_eq!(load_grants(&dir).unwrap().len(), 1);
    write(&dir, "same", &spec("same"));
    write(&dir, "other", &spec("same"));
    let loaded = load_dir(&dir, &Grants::new(), &Factory);
    assert!(loaded.modules.is_empty());
    assert_eq!(loaded.errors.len(), 2);
    assert!(
        loaded
            .errors
            .iter()
            .all(|e| e.message.contains("duplicate"))
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_runtime_is_an_isolated_error() {
    let dir = root("no-runtime");
    write(&dir, "one", &spec("one"));
    write(&dir, "two", &spec("two"));
    let loaded = load_dir(&dir, &Grants::new(), &NoRuntime);
    assert!(loaded.modules.is_empty());
    assert_eq!(loaded.errors.len(), 2);
    assert!(
        loaded
            .errors
            .iter()
            .all(|error| error.message.contains("no runtime"))
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn special_files_are_rejected_before_opening() {
    use std::process::{Command, Stdio};
    let dir = root("fifo");
    if Command::new("mkfifo")
        .arg(dir.join("probe"))
        .status()
        .is_err()
    {
        std::fs::remove_dir_all(dir).unwrap();
        return;
    }
    std::fs::remove_file(dir.join("probe")).unwrap();
    for kind in ["grants", "manifest", "entry"] {
        let module = dir.join("script");
        write(&dir, "script", &spec("script"));
        let path = match kind {
            "grants" => dir.join("grants.yaml"),
            "manifest" => module.join("module.yaml"),
            _ => module.join("main.wasm"),
        };
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        assert!(
            Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let mut writer = Command::new("sh")
            .args(["-c", "printf '{}' > \"$1\"", "sh"])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let error = if kind == "grants" {
            load_grants(&dir).err().unwrap_or_default()
        } else {
            let loaded = load_dir(&dir, &Grants::new(), &Factory);
            loaded
                .errors
                .first()
                .map_or_else(String::new, |error| error.message.clone())
        };
        let _ = writer.kill();
        let _ = writer.wait();
        assert!(error.contains("regular file"), "{kind}: {error}");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(module).unwrap();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn size_symlink_permission_and_invalid_id_are_refused() {
    let dir = root("boundaries");
    let mut invalid_id = spec("badid");
    invalid_id["id"] = json!("INVALID");
    write(&dir, "badid", &invalid_id);
    let mut permission = spec("permission");
    permission["permissions"] =
        json!([{"key":"other.test", "default_level":50, "description":"test"}]);
    write(&dir, "permission", &permission);
    write(&dir, "large_manifest", &spec("large_manifest"));
    std::fs::write(
        dir.join("large_manifest/module.yaml"),
        vec![b' '; 256 * 1024 + 1],
    )
    .unwrap();
    write(&dir, "large_entry", &spec("large_entry"));
    std::fs::write(
        dir.join("large_entry/main.wasm"),
        vec![0; 8 * 1024 * 1024 + 1],
    )
    .unwrap();
    write(&dir, "link", &spec("link"));
    let outside = dir.join("outside");
    std::fs::write(&outside, b"outside").unwrap();
    std::fs::remove_file(dir.join("link/main.wasm")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("link/main.wasm")).unwrap();
    let loaded = load_dir(&dir, &Grants::new(), &Factory);
    assert!(loaded.modules.is_empty());
    assert_eq!(loaded.errors.len(), 5, "{:?}", loaded.errors);
    assert!(loaded.errors.iter().any(|error| error.path.ends_with("permission") && error.message.contains("other.test")));
    std::fs::remove_dir_all(dir).unwrap();
}
