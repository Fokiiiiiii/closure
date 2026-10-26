//! `/automod counters`: looks at and changes the counters, privately. A change made here runs the
//! counter triggers it fires, as an action's change would.

use std::sync::Arc;

use closure_sdk::counters::{CounterDef, Counters};
use closure_sdk::{Args, ChannelId, CommandArgs, CommandCx, ModuleError, UserError, UserId};

use crate::config::Problem;
use crate::context::Who;
use crate::counter::{self, Missing, Op};
use crate::engine::{self, Loaded, State};

#[derive(CommandArgs)]
pub struct View {
    /// The counter
    pub counter: String,
    /// The member the counter is kept for
    pub user: Option<UserId>,
    /// The channel the counter is kept for
    pub channel: Option<ChannelId>,
}

#[derive(CommandArgs)]
pub struct Set {
    /// The counter
    pub counter: String,
    /// The new value
    pub value: i64,
    /// The member the counter is kept for
    pub user: Option<UserId>,
    /// The channel the counter is kept for
    pub channel: Option<ChannelId>,
}

#[derive(CommandArgs)]
pub struct Add {
    /// The counter
    pub counter: String,
    /// How much to add; a negative number takes away
    pub amount: i64,
    /// The member the counter is kept for
    pub user: Option<UserId>,
    /// The channel the counter is kept for
    pub channel: Option<ChannelId>,
}

#[derive(CommandArgs)]
pub struct Reset {
    /// The counter
    pub counter: String,
    /// The member the counter is kept for; with no scope given, every scope is reset
    pub user: Option<UserId>,
    /// The channel the counter is kept for; with no scope given, every scope is reset
    pub channel: Option<ChannelId>,
}

pub async fn view(state: Arc<State>, cx: CommandCx, args: View) -> Result<(), UserError> {
    let loaded = load(&state, &cx)?;
    let def = match lookup(&loaded, &cx, &args.counter) {
        Ok(def) => def,
        Err(text) => return cx.reply_private(text).await,
    };
    let (user, channel) = scope_of(&def, args.user, args.channel);
    if let Some(text) = missing(&cx, &def, user, channel) {
        return cx.reply_private(text).await;
    }
    let counters = counters_of(&cx, &loaded)?;
    let value = counters
        .get(&def.name, channel, user, cx.now())
        .await
        .map_err(counter_error)?;
    reply_value(&cx, &def.name, user, channel, value).await
}

pub async fn set(state: Arc<State>, cx: CommandCx, args: Set) -> Result<(), UserError> {
    change(
        &state,
        &cx,
        &args.counter,
        args.user,
        args.channel,
        Op::Set(args.value),
    )
    .await
}

pub async fn add(state: Arc<State>, cx: CommandCx, args: Add) -> Result<(), UserError> {
    change(
        &state,
        &cx,
        &args.counter,
        args.user,
        args.channel,
        Op::Add(args.amount),
    )
    .await
}

pub async fn reset(state: Arc<State>, cx: CommandCx, args: Reset) -> Result<(), UserError> {
    let loaded = load(&state, &cx)?;
    let def = match lookup(&loaded, &cx, &args.counter) {
        Ok(def) => def,
        Err(text) => return cx.reply_private(text).await,
    };
    let counters = counters_of(&cx, &loaded)?;
    let (user, channel) = scope_of(&def, args.user, args.channel);
    if args.user.is_none() && args.channel.is_none() {
        counters.reset_all(&def.name).await.map_err(counter_error)?;
    } else {
        if let Some(text) = missing(&cx, &def, user, channel) {
            return cx.reply_private(text).await;
        }
        counters
            .reset(&def.name, channel, user)
            .await
            .map_err(counter_error)?;
    }
    let scope = scope_label(&cx, user, channel);
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
