use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use closure_core::module::{ModuleId, ModuleManifest, ModuleRuntime};
use serde::Deserialize;

use crate::runtime::ScriptRuntime;
use crate::{Grants, Limits, ScriptModule, parse_grants};

const MANIFEST_MAX: u64 = 256 * 1024;
const ENTRY_MAX: u64 = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptSpec {
    #[serde(flatten)]
    pub manifest: ModuleManifest,
    pub entry: PathBuf,
    #[serde(default)]
    pub limits: Limits,
}

pub trait RuntimeFactory {
    fn create(
        &self,
        kind: ModuleRuntime,
        entry: &[u8],
    ) -> Result<Option<Arc<dyn ScriptRuntime>>, String>;
}

#[derive(Debug)]
pub struct LoadError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Default)]
pub struct Loaded {
    pub modules: Vec<ScriptModule>,
    pub errors: Vec<LoadError>,
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    read_file_bounded(file, path, max)
}

fn read_file_bounded(file: File, path: &Path, max: u64) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > max {
        return Err(format!("{} exceeds {max} bytes", path.display()));
    }
    Ok(data)
}

pub fn load_grants(dir: &Path) -> Result<Grants, String> {
    let path = dir.join("grants.yaml");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Grants::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    match read_file_bounded(file, &path, MANIFEST_MAX) {
        Ok(bytes) => {
            let text = std::str::from_utf8(&bytes).map_err(|e| e.to_string())?;
            parse_grants(text).map_err(|e| e.to_string())
        }
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn read_spec(dir: &Path) -> Result<(ScriptSpec, Vec<u8>), String> {
    let data = read_bounded(&dir.join("module.yaml"), MANIFEST_MAX)?;
    let text = std::str::from_utf8(&data).map_err(|e| e.to_string())?;
    let spec: ScriptSpec = serde_saphyr::from_str(text).map_err(|e| e.to_string())?;
    if spec.manifest.runtime == ModuleRuntime::Native {
        return Err("native runtime is not allowed".into());
    }
    spec.manifest.validate().map_err(|e| e.to_string())?;
    spec.limits
        .bounded(Limits::default())
        .map_err(str::to_owned)?;
    if spec.entry.as_os_str().is_empty()
        || spec
            .entry
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err("entry must be a relative path inside the script directory".into());
    }
    let root = fs::canonicalize(dir).map_err(|e| e.to_string())?;
    let entry = fs::canonicalize(dir.join(&spec.entry)).map_err(|e| e.to_string())?;
    if !entry.starts_with(root) {
        return Err("entry escapes the script directory".into());
    }
    let bytes = read_bounded(&entry, ENTRY_MAX)?;
    Ok((spec, bytes))
}

pub fn load_dir(dir: &Path, grants: &Grants, runtimes: &dyn RuntimeFactory) -> Loaded {
    let mut loaded = Loaded::default();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return loaded,
        Err(error) => {
            loaded.errors.push(LoadError {
                path: dir.to_owned(),
                message: error.to_string(),
            });
            return loaded;
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) if entry.path().is_dir() => paths.push(entry.path()),
            Ok(_) => {}
            Err(error) => loaded.errors.push(LoadError {
                path: dir.to_owned(),
                message: error.to_string(),
            }),
        }
    }
    paths.sort();
    let mut candidates = Vec::new();
    let mut counts = BTreeMap::<ModuleId, usize>::new();
    for path in paths {
        match read_spec(&path) {
            Ok((spec, bytes)) => {
                *counts.entry(spec.manifest.id.clone()).or_default() += 1;
                candidates.push((path, spec, bytes));
            }
            Err(message) => loaded.errors.push(LoadError { path, message }),
        }
    }
    for (path, spec, bytes) in candidates {
        let result = (|| {
            if counts[&spec.manifest.id] > 1 {
                return Err("duplicate module id".into());
            }
            if path.file_name().and_then(|s| s.to_str()) != Some(spec.manifest.id.as_str()) {
                return Err("directory name does not match module id".into());
            }
            let runtime = runtimes
                .create(spec.manifest.runtime, &bytes)?
                .ok_or_else(|| "this build has no runtime for the script".to_owned())?;
            ScriptModule::new(spec.manifest, grants, runtime, spec.limits)
        })();
        match result {
            Ok(module) => loaded.modules.push(module),
            Err(message) => loaded.errors.push(LoadError { path, message }),
        }
    }
    loaded
}
