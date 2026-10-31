//! Developer tooling for closure: the generated module reference and the module scaffold.
//!
//! `docs` writes `docs/reference/<module>.md` from the shipped modules; `docs --check` writes
//! nothing and fails when a file on disk differs from what would be generated.

mod clear;
mod docs;
mod replay;
mod scaffold;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use closure::modules;
use closure_core::registry::ModuleRegistry;
use closure_i18n::TranslatorBuilder;

#[derive(Debug, Parser)]
#[command(about = "Developer tooling for closure")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Writes docs/reference/<module>.md for every shipped module.
    Docs {
        /// Writes nothing; fails when a file differs from what would be generated.
        #[arg(long)]
        check: bool,
    },
    /// Creates crates/closure-mod-<id> from the templates and adds it to the workspace.
    NewModule {
        /// The module id: [a-z0-9_-], starting with a letter, at most 32 characters.
        id: String,
        /// Prints the files that would be created, without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Posts the messages recorded by a test run (`CLOSURE_DUMP_DIR`) to a test channel.
    /// Needs `DISCORD_TOKEN`, from the environment or from `.env`. See docs/preview.md.
    Replay {
        /// The directory with the `.jsonl` dumps of the test run.
        #[arg(long)]
        dir: PathBuf,
        /// The test channel to post to.
        #[arg(long)]
        channel: u64,
    },
    /// Deletes the bot's own messages in a test channel posted at or after `--after`, so that a
    /// replay can start over. Needs `DISCORD_TOKEN`.
    Clear {
        /// The test channel to clear.
        #[arg(long)]
        channel: u64,
        /// Unix time in seconds; the bot's messages posted before it are kept.
        #[arg(long)]
        after: i64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli.command) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("closure-xtask: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the command; `Ok(false)` means it ran and found a problem to report.
fn run(command: &Command) -> Result<bool> {
    match command {
        Command::Docs { check } => docs(*check),
        Command::NewModule { id, dry_run } => new_module(id, *dry_run),
        Command::Replay { dir, channel } => replay::run(dir, *channel),
        Command::Clear { channel, after } => clear::run(*channel, *after),
    }
}

/// The root of the workspace this tool was built from.
fn workspace_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    root.canonicalize().unwrap_or(root)
}

fn docs(check: bool) -> Result<bool> {
    let registry = ModuleRegistry::new(modules()?).context("the set of modules is inconsistent")?;
    let mut catalogs = TranslatorBuilder::new()?;
    catalogs.add_registry(&registry)?;
    let translator = catalogs.build();
    let manifests: Vec<_> = registry.iter().map(|module| module.manifest()).collect();
    let expected = docs::expected_files(manifests, &translator);

    let directory = workspace_root().join("docs").join("reference");
    let actual = read_markdown(&directory)?;
    if check {
        let differences = docs::differences(&expected, &actual);
        for difference in &differences {
            eprintln!("{difference} (run `cargo run -p closure-xtask -- docs`)");
        }
        return Ok(differences.is_empty());
    }

    fs::create_dir_all(&directory)?;
    for (name, content) in &expected {
        if actual.get(name) != Some(content) {
            fs::write(directory.join(name), content)?;
        }
    }
    for name in actual.keys().filter(|name| !expected.contains_key(*name)) {
        fs::remove_file(directory.join(name))?;
    }
    Ok(true)
}

/// Creates `crates/closure-mod-<id>` and its workspace entry; a dry run only prints the listing.
fn new_module(id: &str, dry_run: bool) -> Result<bool> {
    let root = workspace_root();
    let names = scaffold::names(id)?;
    let files = scaffold::render(&names);
    let cargo_path = root.join("Cargo.toml");
    let workspace =
        scaffold::add_workspace_dependency(&fs::read_to_string(&cargo_path)?, &names.package)?;
    let crate_dir = root.join("crates").join(&names.package);
    if crate_dir.exists() {
        bail!("{} already exists; choose another id", crate_dir.display());
    }
    if dry_run {
        print!("{}", scaffold::listing(&files));
        return Ok(true);
    }
    let mut written = Vec::new();
    for (path, content) in &files {
        let target = crate_dir.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&target, content)?;
        written.push(target);
    }
    scaffold::format_sources(&written)?;
    fs::write(&cargo_path, workspace)?;
    eprintln!("created {}", crate_dir.display());
    eprintln!(
        "next: add `{}.workspace = true` to crates/closure/Cargo.toml",
        names.package
    );
    eprintln!(
        "next: add `Arc::new({}::module()?),` to modules() in crates/closure/src/lib.rs",
        names.library
    );
    Ok(true)
}

/// The Markdown files in `directory`, by file name; none when the directory does not exist yet.
fn read_markdown(directory: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    if !directory.exists() {
        return Ok(files);
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_some_and(|extension| extension == "md") {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            files.insert(name, fs::read_to_string(&path)?);
        }
    }
    Ok(files)
}
