//! Discord integration built on Twilight.
//!
//! - [`TwilightApi`] performs the [`Action`](closure_core::action::Action)s modules request,
//!   normalising errors, checking the role hierarchy first and retrying safe reads.
//! - [`Dispatcher`] delivers events to modules with per-guild ordering, limits, timeouts, panic
//!   isolation and a circuit breaker.
//! - [`run_gateway`] connects the shards, feeds the cache and the dispatcher, and shuts down
//!   gracefully.

mod api;
mod breaker;
mod commands;
mod convert;
mod dispatcher;
mod error;
mod gateway;
pub mod hierarchy;
mod jobs;
mod text_commands;

pub use api::{RetryPolicy, TwilightApi, joined_guilds};
pub use breaker::BreakerConfig;
pub use commands::{CommandBuildError, CommandScope, build_commands, register_commands};
pub use dispatcher::{DispatchError, Dispatcher, DispatcherConfig, JobOutcome};
pub use gateway::{
    GatewayConfig, GatewayError, GatewayHandle, GatewayHealth, GuildHooks, NoHooks,
    gateway_intents, new_cache, start_gateway,
};
pub use jobs::{run_due, run_jobs};
