//! Shared contract of closure.
//!
//! Everything a module needs lives here: the [`module::Module`] trait, the stable
//! [`event::Event`] model, the [`capability::Capability`] set and the serializable
//! [`action::Action`] boundary through which a module talks to Discord. This crate
//! depends on `twilight-model` for identifier and payload types only, so native modules,
//! tests and future Lua/WASM adapters can all be written against it.

pub mod action;
pub mod capability;
pub mod client;
pub mod error;
pub mod event;
pub mod interaction;
pub mod locale;
pub mod module;
pub mod registry;
pub mod settings;
pub mod testing;
pub mod throttle;

pub use twilight_model::gateway::Intents;
