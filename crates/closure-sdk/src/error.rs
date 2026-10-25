//! What a command handler can fail with.

use std::time::Duration;

use closure_core::error::{DiscordError, ModuleError};
use closure_core::locale::{ArgValue, Args};
use closure_core::permission::Decision;

use crate::args::ArgError;

/// An error a command handler returns.
///
/// The ones that are the user's business ([`Self::message`], a missing permission, a cooldown, a
/// bad option) are answered by the SDK in the user's language, privately, and are no failure of
/// the module. The others are the module's: they reach the dispatcher, which tells the user
/// that something went wrong and counts the failure.
#[derive(Debug)]
pub enum UserError {
    /// A message from the module's own catalog.
    Message {
        key: String,
        args: Args,
    },
    BadArgument(ArgError),
    PermissionDenied(Decision),
    Cooldown {
        remaining: Duration,
    },
    Module(ModuleError),
}

impl UserError {
    /// Answers with the message `key` of the module's catalog.
    #[must_use]
    pub fn message(key: impl Into<String>) -> Self {
        Self::Message {
            key: key.into(),
            args: Args::new(),
        }
    }

    /// Adds a value for the message. Does nothing to the other kinds of error.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<ArgValue>) -> Self {
        if let Self::Message { args, .. } = &mut self {
            *args = std::mem::take(args).with(name, value);
        }
        self
    }
}

impl From<ModuleError> for UserError {
    fn from(error: ModuleError) -> Self {
        Self::Module(error)
    }
}

impl From<DiscordError> for UserError {
    fn from(error: DiscordError) -> Self {
        Self::Module(ModuleError::Discord(error))
    }
}

impl From<ArgError> for UserError {
    fn from(error: ArgError) -> Self {
        Self::BadArgument(error)
    }
}
