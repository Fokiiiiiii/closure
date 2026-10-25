use std::time::Duration;

use thiserror::Error;

use crate::capability::Capability;
use crate::module::ModuleId;

/// Errors raised while assembling modules at startup. All of them are fatal by design:
/// a mis-wired module set must never reach the gateway.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoreError {
    #[error(
        "invalid module id `{0}`: expected 1-32 chars of [a-z0-9_-], starting with a lowercase letter"
    )]
    InvalidModuleId(String),
    #[error("duplicate module id `{0}`")]
    DuplicateModule(ModuleId),
    #[error("module `{module}` depends on unknown module `{dependency}`")]
    UnknownDependency {
        module: ModuleId,
        dependency: ModuleId,
    },
    #[error("dependency cycle involving module `{0}`")]
    DependencyCycle(ModuleId),
    #[error("invalid level {0}: expected a number from 1 to 100")]
    InvalidLevel(u8),
    #[error(
        "invalid permission key `{0}`: expected 2-4 dot-separated parts of [a-z][a-z0-9_-]*, 64 chars at most"
    )]
    InvalidPermissionKey(String),
    #[error("module `{module}` declares the permission `{key}`, which does not start with its id")]
    PermissionOutsideModule { module: ModuleId, key: String },
    #[error("invalid subscription to `{kind}` in module `{module}`: {reason}")]
    InvalidSubscription {
        module: ModuleId,
        kind: String,
        reason: String,
    },
    #[error("module `{module}` declares the permission `{key}` twice")]
    DuplicatePermission { module: ModuleId, key: String },
    #[error("command `{command}` is declared by both `{first}` and `{second}`")]
    CommandCollision {
        command: String,
        first: ModuleId,
        second: ModuleId,
    },
    #[error("invalid command `{command}` in module `{module}`: {reason}")]
    InvalidCommand {
        module: ModuleId,
        command: String,
        reason: String,
    },
}

/// Errors a module returns from its handlers.
#[derive(Debug, Error)]
pub enum ModuleError {
    #[error("invalid module configuration: {0}")]
    Config(String),
    #[error(transparent)]
    Discord(#[from] DiscordError),
    #[error("{0}")]
    Other(String),
}

impl ModuleError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

/// Normalised failure of a Discord operation. Implementations of
/// [`DiscordApi`](crate::client::DiscordApi) map transport and HTTP errors onto these
/// variants so modules never depend on a particular HTTP library.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum DiscordError {
    /// The action was rejected locally before any request was made.
    #[error("invalid action: {0}")]
    Invalid(String),
    /// The calling module was not granted the capability the action needs.
    #[error("capability `{0}` is not granted to this module")]
    CapabilityDenied(Capability),
    /// The bot lacks a permission, or Discord answered 403.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The target (or the bot) sits too high or too low in the role hierarchy.
    #[error("role hierarchy violation: {0}")]
    HierarchyViolation(String),
    #[error("not found")]
    NotFound,
    /// The user does not accept direct messages from the bot (Discord's error 50007). Retrying
    /// cannot change that, and it is no fault of the module.
    #[error("the user does not accept direct messages from this bot")]
    DirectMessagesClosed,
    /// The member is not connected to a voice channel, so Discord cannot mute it there (error
    /// 40032). Retrying cannot change that until the member joins voice.
    #[error("the member is not connected to voice")]
    NotInVoice,
    #[error("rate limited{}", .retry_after.map(|d| format!(" (retry after {d:?})")).unwrap_or_default())]
    RateLimited { retry_after: Option<Duration> },
    /// A failure that may succeed on retry: 5xx, network or timeout.
    #[error("transient discord failure: {0}")]
    Transient(String),
    /// A failure that retrying cannot fix.
    #[error("discord request failed: {0}")]
    Fatal(String),
}

impl DiscordError {
    /// Whether repeating the identical request may succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient(_) | Self::RateLimited { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_direct_message_is_not_worth_repeating() {
        assert!(!DiscordError::DirectMessagesClosed.is_retryable());
    }

    #[test]
    fn a_member_outside_voice_is_not_worth_repeating() {
        assert!(!DiscordError::NotInVoice.is_retryable());
    }
}
