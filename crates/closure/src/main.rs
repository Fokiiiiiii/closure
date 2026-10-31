//! closure: the composition root.
//!
//! This crate only wires the others together: it reads the process configuration, builds the
//! modules, starts the gateway, the dispatcher and the HTTP API, and shuts them down in order.
//! Behaviour lives in the library crates, where it can be tested without a process.

mod app;
mod args;
mod changes;
mod hooks;
mod sync;

use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    // A missing `.env` is normal; variables already set in the environment always win.
    let _ = dotenvy::dotenv();
    let args = args::Args::parse();
    args.init_logging();

    match app::run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
