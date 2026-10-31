//! `/automod counters`: looks at and changes the counters, privately. A change made here runs the
//! counter triggers it fires, as an action's change would.

use std::sync::Arc;

use closure_sdk::counters::{CounterDef, Counters};
use closure_sdk::{
    Args, AutocompleteCx, ChannelId, Choice, CommandArgs, CommandCx, ModuleError, Span, UserError,
    UserId,
};

use crate::antiraid::{self, Setter, Tally};
use crate::config::Problem;
use crate::context::{Context, Who};
use crate::counter::{self, Missing, Op};
use crate::engine::{self, Loaded, State};

/// `/automod counter`: a counter, what to do with it, and the member or channel it is kept for.
#[derive(CommandArgs)]
pub struct CounterArgs {
    /// The counter
    pub counter: String,
    /// What to do: view the counter, set it to a value, add to it, or reset it to its start
    #[arg(choices = ["view", "set", "add", "reset"])]
    pub action: String,
    /// The value to set, or the amount to add (a negative number takes away); not needed for view or reset
    pub value: Option<i64>,
    /// The member the counter is kept for; with reset and no scope given, every scope is reset
    pub user: Option<UserId>,
    /// The channel the counter is kept for; with reset and no scope given, every scope is reset
    pub channel: Option<ChannelId>,
}

/// `/automod antiraid set`: the level to raise or lower the server to, and for how long.
#[derive(CommandArgs)]
pub struct AntiraidSet {
    /// The level to raise or lower the server to
    #[arg(autocomplete)]
    pub level: String,
    /// How long the level lasts, such as 30m or 2h; the calm level comes back after it
    pub duration: Option<Span>,
}

#[derive(CommandArgs)]
pub struct AntiraidRelease {
    /// The member to take out of the quarantine
    pub user: UserId,
}

/// `/automod counter`: looks at, sets, adds to, or resets a counter, as the action says.
pub async fn counter(state: Arc<State>, cx: CommandCx, args: CounterArgs) -> Result<(), UserError> {
    let CounterArgs {
        counter,
        action,
        value,
        user,
        channel,
    } = args;
    match (action.as_str(), value) {
        ("view", _) => view(&state, &cx, &counter, user, channel).await,
        ("reset", _) => reset(&state, &cx, &counter, user, channel).await,
        ("set", Some(value)) => change(&state, &cx, &counter, user, channel, Op::Set(value)).await,
        ("add", Some(amount)) => {
            change(&state, &cx, &counter, user, channel, Op::Add(amount)).await
        }
        ("set" | "add", None) => {
            cx.reply_private_t("counter-value-needed", &Args::new())
                .await
        }
        _ => {
            cx.reply_private_t("counter-action-unknown", &Args::new())
                .await
        }
    }
}

async fn view(
    state: &State,
    cx: &CommandCx,
    name: &str,
    user: Option<UserId>,
    channel: Option<ChannelId>,
) -> Result<(), UserError> {
    let loaded = load(state, cx)?;
    let def = match lookup(&loaded, cx, name) {
        Ok(def) => def,
        Err(text) => return cx.reply_private(text).await,
    };
    let (user, channel) = scope_of(&def, user, channel);
    if let Some(text) = missing(cx, &def, user, channel) {
        return cx.reply_private(text).await;
    }
    let counters = counters_of(cx, &loaded)?;
    let value = counters
        .get(&def.name, channel, user, cx.now())
        .await
        .map_err(counter_error)?;
    reply_value(cx, &def.name, user, channel, value).await
}

async fn reset(
    state: &State,
    cx: &CommandCx,
    name: &str,
    user: Option<UserId>,
    channel: Option<ChannelId>,
) -> Result<(), UserError> {
    let loaded = load(state, cx)?;
    let def = match lookup(&loaded, cx, name) {
        Ok(def) => def,
        Err(text) => return cx.reply_private(text).await,
    };
    let counters = counters_of(cx, &loaded)?;
    let (scoped_user, scoped_channel) = scope_of(&def, user, channel);
    if user.is_none() && channel.is_none() {
        counters.reset_all(&def.name).await.map_err(counter_error)?;
    } else {
        if let Some(text) = missing(cx, &def, scoped_user, scoped_channel) {
            return cx.reply_private(text).await;
        }
        counters
            .reset(&def.name, scoped_channel, scoped_user)
            .await
            .map_err(counter_error)?;
    }
    let scope = scope_label(cx, scoped_user, scoped_channel);
    let text = cx.t(
        "counter-reset",
        &Args::new()
            .with("counter", def.name.as_str())
            .with("scope", scope),
    );
    cx.reply_private(text).await
}

/// Sets or adds to a counter. The triggers the change fires run through the rules, as an action's do.
async fn change(
    state: &State,
    cx: &CommandCx,
    name: &str,
    user: Option<UserId>,
    channel: Option<ChannelId>,
    op: Op,
) -> Result<(), UserError> {
    let loaded = load(state, cx)?;
    let def = match lookup(&loaded, cx, name) {
        Ok(def) => def,
        Err(text) => return cx.reply_private(text).await,
    };
    let (user, channel) = scope_of(&def, user, channel);
    if let Some(text) = missing(cx, &def, user, channel) {
        return cx.reply_private(text).await;
    }
    let outcome = counter::change(
        cx.ctx(),
        &loaded.setup.counters,
        &def.name,
        channel,
        user,
        op,
    )
    .await
    .map_err(UserError::Module)?;
    let who = user.map(|id| Who::bare(id, id.get().to_string(), false));
    let fired = counter::fired(&outcome, &def.name, channel, who.as_ref());
    engine::after_command(state, cx.ctx(), fired)
        .await
        .map_err(UserError::Module)?;
    reply_value(cx, &def.name, user, channel, outcome.new).await
}

fn load(state: &State, cx: &CommandCx) -> Result<Arc<Loaded>, UserError> {
    state.load(cx.ctx()).map_err(UserError::Module)
}

fn counters_of(cx: &CommandCx, loaded: &Loaded) -> Result<Counters, UserError> {
    Counters::new(cx.ctx().store(), &loaded.setup.counters).map_err(counter_error)
}

fn counter_error(error: impl std::fmt::Display) -> UserError {
    UserError::Module(ModuleError::other(error.to_string()))
}

/// The counter called `name`, or the text that says why it cannot be used.
fn lookup(loaded: &Loaded, cx: &CommandCx, name: &str) -> Result<CounterDef, String> {
    if let Some(problem) = &loaded.setup.counters_problem {
        return Err(match problem {
            Problem::Counters(reason) => cx.t(
                "problem-counters-sdk",
                &Args::new().with("reason", reason.as_str()),
            ),
            Problem::Key(key) => cx.t(key, &Args::new()),
            Problem::Shape(error) => error.clone(),
        });
    }
    counter::find(&loaded.setup.counters, name)
        .cloned()
        .ok_or_else(|| cx.t("counter-unknown", &Args::new().with("counter", name)))
}

/// The member and the channel a counter is kept for: only the ones its definition uses.
fn scope_of(
    def: &CounterDef,
    user: Option<UserId>,
    channel: Option<ChannelId>,
) -> (Option<UserId>, Option<ChannelId>) {
    (
        if def.per_user { user } else { None },
        if def.per_channel { channel } else { None },
    )
}

/// What a change of the counter lacks, as the text that says so.
fn missing(
    cx: &CommandCx,
    def: &CounterDef,
    user: Option<UserId>,
    channel: Option<ChannelId>,
) -> Option<String> {
    match counter::check_scope(def, channel, user) {
        Ok(()) => None,
        Err(Missing::User) => Some(cx.t("counter-needs-user", &Args::new())),
        Err(Missing::Channel) => Some(cx.t("counter-needs-channel", &Args::new())),
    }
}

fn scope_label(cx: &CommandCx, user: Option<UserId>, channel: Option<ChannelId>) -> String {
    let member = user.map(|id| format!("<@{}>", id.get()));
    let place = channel.map(|id| format!("<#{}>", id.get()));
    match (member, place) {
        (None, None) => cx.t("scope-all", &Args::new()),
        (Some(member), None) => cx.t("scope-user", &Args::new().with("user", member)),
        (None, Some(place)) => cx.t("scope-channel", &Args::new().with("channel", place)),
        (Some(member), Some(place)) => cx.t(
            "scope-both",
            &Args::new().with("user", member).with("channel", place),
        ),
    }
}

async fn reply_value(
    cx: &CommandCx,
    name: &str,
    user: Option<UserId>,
    channel: Option<ChannelId>,
    value: i64,
) -> Result<(), UserError> {
    let text = cx.t(
        "counter-view",
        &Args::new()
            .with("counter", name)
            .with("scope", scope_label(cx, user, channel))
            .with("value", value),
    );
    cx.reply_private(text).await
}

// ---- /automod antiraid ----

/// The level names that start with what the user typed, for the `set` command.
pub fn level_choices(state: &State, cx: &AutocompleteCx) -> Result<Vec<Choice>, UserError> {
    let loaded = state.load(cx.ctx()).map_err(UserError::Module)?;
    let Some(antiraid) = loaded.setup.antiraid.as_ref() else {
        return Ok(Vec::new());
    };
    let typed = cx.focused().to_owned();
    Ok(antiraid
        .levels
        .iter()
        .filter(|level| level.starts_with(typed.as_str()))
        .map(|level| Choice::new(level.as_str(), level.as_str()))
        .collect())
}

pub async fn antiraid_view(state: Arc<State>, cx: CommandCx, (): ()) -> Result<(), UserError> {
    let loaded = load(&state, &cx)?;
    let Some(antiraid) = loaded.setup.antiraid.as_ref() else {
        return antiraid_off(&cx).await;
    };
    let store = cx.ctx().store();
    let (level, _) = antiraid::level(store, antiraid.calm())
        .await
        .map_err(UserError::Module)?;
    let quarantined = antiraid::quarantined_count(store)
        .await
        .map_err(UserError::Module)?;
    let by = match level.set_by {
        Setter::Bot => cx.t("antiraid-by-rule", &Args::new()),
        Setter::User { id } => format!("<@{id}>"),
    };
    let until = level.until.map_or_else(
        || cx.t("antiraid-no-end", &Args::new()),
        |at| cx.t("antiraid-until", &Args::new().with("at", at)),
    );
    let mut text = cx.t(
        "antiraid-view",
        &Args::new()
            .with("level", level.name.as_str())
            .with("since", format!("<t:{}:f>", level.set_at))
            .with("by", by)
            .with("until", until)
            .with("quarantined", count(quarantined)),
    );
    if let Some(gap) = level.incomplete() {
        let reason = cx.t(antiraid::gap_key(gap), &Args::new());
        text.push('\n');
        text.push_str(&cx.t("antiraid-incomplete", &Args::new().with("reason", reason)));
    }
    if let Some(quarantine) = antiraid
        .effects
        .get(&level.name)
        .and_then(|effect| effect.quarantine)
    {
        let key = if antiraid::quarantine_is_ready(store, quarantine).await {
            "antiraid-quarantine-ready"
        } else {
            "antiraid-quarantine-syncing"
        };
        text.push('\n');
        text.push_str(&cx.t(key, &Args::new()));
    }
    cx.reply_private(text).await
}

pub async fn antiraid_set(
    state: Arc<State>,
    cx: CommandCx,
    args: AntiraidSet,
) -> Result<(), UserError> {
    let duration = match &args.duration {
        Some(span) => Some(span.resolve(&cx)?),
        None => None,
    };
    let by = Setter::User {
        id: cx.user().id.get(),
    };
    set_and_reply(&state, &cx, &args.level, by, duration).await
}

pub async fn antiraid_clear(state: Arc<State>, cx: CommandCx, (): ()) -> Result<(), UserError> {
    let loaded = load(&state, &cx)?;
    let Some(antiraid) = loaded.setup.antiraid.as_ref() else {
        return antiraid_off(&cx).await;
    };
    let calm = antiraid.calm().to_owned();
    let by = Setter::User {
        id: cx.user().id.get(),
    };
    set_and_reply(&state, &cx, &calm, by, None).await
}

pub async fn antiraid_release(
    state: Arc<State>,
    cx: CommandCx,
    args: AntiraidRelease,
) -> Result<(), UserError> {
    let tally = antiraid::release(cx.ctx(), &state.memory, args.user)
        .await
        .map_err(UserError::Module)?;
    reply_tally(&cx, "antiraid-released", tally).await
}

pub async fn antiraid_release_all(
    state: Arc<State>,
    cx: CommandCx,
    (): (),
) -> Result<(), UserError> {
    let tally = antiraid::release_all(cx.ctx(), &state.memory)
        .await
        .map_err(UserError::Module)?;
    reply_tally(&cx, "antiraid-released-all", tally).await
}

/// Changes the level and runs the change through the rules, then says what it did.
async fn set_and_reply(
    state: &State,
    cx: &CommandCx,
    level: &str,
    by: Setter,
    duration: Option<std::time::Duration>,
) -> Result<(), UserError> {
    let loaded = load(state, cx)?;
    let Some(antiraid) = loaded.setup.antiraid.as_ref() else {
        return antiraid_off(cx).await;
    };
    let change = antiraid::set_level(
        cx.ctx(),
        antiraid,
        level,
        by,
        duration,
        None,
        loaded.config.dry_run,
    )
    .await
    .map_err(UserError::Module)?;
    let Some(change) = change else {
        let text = cx.t("antiraid-already", &Args::new().with("level", level));
        return cx.reply_private(text).await;
    };
    // Making the effects of the level that is set again is no change for the rules to see.
    if !change.reapplied() {
        engine::after_command(
            state,
            cx.ctx(),
            vec![Context::antiraid(&change.new, &change.old)],
        )
        .await
        .map_err(UserError::Module)?;
    }
    let args = Args::new()
        .with("old", change.old.as_str())
        .with("new", change.new.as_str())
        .with("changed", count(change.tally.changed))
        .with("failed", count(change.tally.failed));
    let mut text = if change.reapplied() {
        cx.t("antiraid-reapplied", &args)
    } else {
        cx.t("antiraid-set-done", &args)
    };
    if let Some(gap) = change.gap {
        let reason = cx.t(antiraid::gap_key(gap), &Args::new());
        text.push('\n');
        text.push_str(&cx.t("antiraid-incomplete", &Args::new().with("reason", reason)));
    }
    cx.reply_private(text).await
}

async fn reply_tally(cx: &CommandCx, key: &str, tally: Tally) -> Result<(), UserError> {
    let text = cx.t(
        key,
        &Args::new()
            .with("changed", count(tally.changed))
            .with("failed", count(tally.failed)),
    );
    cx.reply_private(text).await
}

async fn antiraid_off(cx: &CommandCx) -> Result<(), UserError> {
    cx.reply_private(cx.t("antiraid-off", &Args::new())).await
}

fn count(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
