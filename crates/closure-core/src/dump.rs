//! Records the actions of a test run, so that `closure-xtask replay` can show them in Discord.
//!
//! When `CLOSURE_DUMP_DIR` is set, every action the test double
//! [`MockApi`](crate::testing::MockApi) records is also appended to `<dir>/<process id>.jsonl`, one
//! JSON object per line. Without the variable nothing is written and no file is created.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use crate::action::Action;

/// The environment variable that turns recording on: the directory the dump files go to.
pub const DUMP_DIR_VARIABLE: &str = "CLOSURE_DUMP_DIR";

/// Keeps the lines of this process from interleaving when tests write at the same time.
static WRITES: Mutex<()> = Mutex::new(());

/// Appends `action` to this process's dump when [`DUMP_DIR_VARIABLE`] is set and not empty.
///
/// # Panics
///
/// Panics when recording is on and the line cannot be written: a preview that silently lost
/// actions would show less than the tests did.
pub fn record(action: &Action) {
    let dir = std::env::var_os(DUMP_DIR_VARIABLE).filter(|value| !value.is_empty());
    if let Err(error) = record_in(dir.as_deref().map(Path::new), action) {
        panic!("{DUMP_DIR_VARIABLE}: cannot record an action: {error}");
    }
}

/// The dump file of this process inside `dir`.
#[must_use]
pub fn dump_file(dir: &Path) -> PathBuf {
    dir.join(format!("{}.jsonl", std::process::id()))
}

/// Appends `action` as one JSON line to the dump file in `dir`; `None` records nothing.
///
/// # Errors
///
/// Returns the error of creating the directory or writing the file.
pub fn record_in(dir: Option<&Path>, action: &Action) -> io::Result<()> {
    let Some(dir) = dir else {
        return Ok(());
    };
    let mut line = line_of(action);
    line.push('\n');
    fs::create_dir_all(dir)?;
    let _guard = WRITES.lock().unwrap_or_else(PoisonError::into_inner);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dump_file(dir))?;
    file.write_all(line.as_bytes())
}

/// The action as one JSON object, with the name of the test thread that made it under `test`.
fn line_of(action: &Action) -> String {
    let mut value = serde_json::to_value(action)
        .unwrap_or_else(|error| panic!("an action serialises to JSON: {error}"));
    if let Some(object) = value.as_object_mut() {
        let test = std::thread::current().name().unwrap_or_default().to_owned();
        object.insert("test".to_owned(), test.into());
    }
    value.to_string()
}
