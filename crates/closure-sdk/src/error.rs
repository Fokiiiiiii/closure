//! What a command handler can fail with.

use std::time::Duration;

use closure_core::cases::CaseError;
use closure_core::error::{DiscordError, ModuleError};
use closure_core::locale::{ArgValue, Args};
use closure_core::permission::Decision;
use closure_core::schedule::ScheduleError;
use closure_core::storage::StorageError;

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
    /// A message from the SDK's catalog (`core`), such as the refusals of a punishment.
    Sdk {
        key: String,
        args: Args,
    },
    BadArgument(ArgError),
    PermissionDenied(Decision),
    Cooldown {
        remaining: Duration,
    },
    /// The target of a moderation action may not be acted on (see `Cx::check_target`).
    Target(TargetProblem),
    Module(ModuleError),
}

/// Why a user may not be the target of a moderation action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetProblem {
    SelfTarget,
    Bot,
    Owner,
    Higher,
}

impl TargetProblem {
    /// The message of the SDK's catalog that tells the user.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::SelfTarget => "sdk-target-self",
            Self::Bot => "sdk-target-bot",
            Self::Owner => "sdk-target-owner",
            Self::Higher => "sdk-target-higher",
        }
    }
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

    /// Answers with the message `key` of the SDK's catalog.
    #[must_use]
    pub fn sdk(key: impl Into<String>) -> Self {
        Self::Sdk {
            key: key.into(),
            args: Args::new(),
        }
    }

    /// Adds a value for the message. Does nothing to the other kinds of error.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<ArgValue>) -> Self {
        if let Self::Message { args, .. } | Self::Sdk { args, .. } = &mut self {
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

impl From<StorageError> for UserError {
    fn from(error: StorageError) -> Self {
        Self::Module(ModuleError::other(error.to_string()))
    }
}

impl From<CaseError> for UserError {
    fn from(error: CaseError) -> Self {
        Self::Module(ModuleError::other(error.to_string()))
    }
}

impl From<ScheduleError> for UserError {
    fn from(error: ScheduleError) -> Self {
        Self::Module(ModuleError::other(error.to_string()))
    }
}

impl From<ArgError> for UserError {
    fn from(error: ArgError) -> Self {
        Self::BadArgument(error)
    }
}
