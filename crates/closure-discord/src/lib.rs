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
mod dispatcher;
mod error;
pub mod hierarchy;

pub use api::{RetryPolicy, TwilightApi};
pub use breaker::BreakerConfig;
pub use dispatcher::{DispatchError, Dispatcher, DispatcherConfig};
