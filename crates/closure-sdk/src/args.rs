//! The options of a slash command, as typed values.

use std::time::Duration;

use closure_core::event::{ChannelId, CommandInvocation, CommandOptionValue, RoleId, UserId};
use closure_core::module::{OptionKind, OptionSpec};

use crate::duration;

/// The options of a command, written as a struct (`#[derive(CommandArgs)]`) or by hand.
pub trait CommandArgs: Sized + Send + 'static {
    /// What is published to Discord.
    fn options() -> Vec<OptionSpec>;

    /// The values of one invocation.
    fn parse(invocation: &CommandInvocation) -> Result<Self, ArgError>;
}

/// A command without options.
impl CommandArgs for () {
    fn options() -> Vec<OptionSpec> {
        Vec::new()
    }

    fn parse(_: &CommandInvocation) -> Result<Self, ArgError> {
        Ok(())
    }
}

/// A type an option can have.
pub trait OptionType: Sized {
    const KIND: OptionKind;

    fn from_value(value: &CommandOptionValue) -> Option<Self>;
}

macro_rules! option_type {
    ($ty:ty, $kind:ident, $variant:ident) => {
        impl OptionType for $ty {
            const KIND: OptionKind = OptionKind::$kind;

            fn from_value(value: &CommandOptionValue) -> Option<Self> {
                match value {
                    CommandOptionValue::$variant(inner) => Some(inner.clone()),
                    _ => None,
                }
            }
        }
    };
}

option_type!(String, String, String);
option_type!(i64, Integer, Integer);
option_type!(f64, Number, Number);
option_type!(bool, Boolean, Boolean);
option_type!(UserId, User, User);
option_type!(ChannelId, Channel, Channel);
option_type!(RoleId, Role, Role);

/// A duration typed as words (`1h30m`), see [`crate::duration::parse`].
impl OptionType for Duration {
    const KIND: OptionKind = OptionKind::String;

    fn from_value(value: &CommandOptionValue) -> Option<Self> {
        match value {
            CommandOptionValue::String(text) => duration::parse(text).ok(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgProblem {
    Missing,
    WrongType,
}

/// An option that is missing or of another type than the command declared. Discord does not
/// send that, so it means the published command and the code have drifted apart.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("option `{option}` is {}", match .problem { ArgProblem::Missing => "missing", ArgProblem::WrongType => "of the wrong type" })]
pub struct ArgError {
    pub option: String,
    pub problem: ArgProblem,
}

/// The value of a required option.
pub fn required<T: OptionType>(invocation: &CommandInvocation, name: &str) -> Result<T, ArgError> {
    optional(invocation, name)?.ok_or_else(|| ArgError {
        option: name.to_owned(),
        problem: ArgProblem::Missing,
    })
}

/// The value of an option that may be left out.
pub fn optional<T: OptionType>(
    invocation: &CommandInvocation,
    name: &str,
) -> Result<Option<T>, ArgError> {
    invocation
        .option(name)
        .map(|value| {
            T::from_value(value).ok_or_else(|| ArgError {
                option: name.to_owned(),
                problem: ArgProblem::WrongType,
            })
        })
        .transpose()
}
