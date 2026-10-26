//! The counters as the actions and the commands change them: the scope a counter is kept in, the
//! change itself, and the contexts of the triggers that the change fired.

use closure_sdk::counters::{Change, CounterDef, Counters};
use closure_sdk::{ChannelId, EventContext, ModuleError, UserId};

use crate::context::{Context, Source, Who};

/// How a counter changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add(i64),
    Set(i64),
}

/// What a change of a counter lacks: the channel or the user its scope needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Channel,
    User,
}

/// The definition of the counter called `name`.
pub fn find<'a>(defs: &'a [CounterDef], name: &str) -> Option<&'a CounterDef> {
    defs.iter().find(|def| def.name == name)
}

/// Whether a change of the counter has the channel and the user its scope needs.
pub fn check_scope(
    def: &CounterDef,
    channel: Option<ChannelId>,
    user: Option<UserId>,
) -> Result<(), Missing> {
    if def.per_channel && channel.is_none() {
        return Err(Missing::Channel);
    }
    if def.per_user && user.is_none() {
        return Err(Missing::User);
    }
    Ok(())
}

/// Changes a counter for a scope, and says which of its triggers the change fired.
pub async fn change(
    ctx: &EventContext,
    defs: &[CounterDef],
    name: &str,
    channel: Option<ChannelId>,
    user: Option<UserId>,
    op: Op,
) -> Result<Change, ModuleError> {
    let counters =
        Counters::new(ctx.store(), defs).map_err(|error| ModuleError::other(error.to_string()))?;
    let now = ctx.now();
    let result = match op {
        Op::Add(delta) => counters.add(name, channel, user, delta, now).await,
        Op::Set(value) => counters.set(name, channel, user, value, now).await,
    };
    result.map_err(|error| ModuleError::other(error.to_string()))
}

/// The contexts of the triggers a change fired: each one is a counter trigger that rules can read,
/// for the member and the channel the change was for.
#[must_use]
pub fn fired(
    change: &Change,
    name: &str,
    channel: Option<ChannelId>,
    who: Option<&Who>,
) -> Vec<Context> {
    change
        .fired
        .iter()
        .map(|fired| Context {
            user: who.cloned(),
            channel,
            message: None,
            source: Source::Counter {
                counter: name.to_owned(),
                trigger: fired.trigger.clone(),
                reverse: fired.reverse,
            },
            created: false,
        })
        .collect()
}
