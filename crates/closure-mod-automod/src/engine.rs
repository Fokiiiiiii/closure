//! The pipeline of automod. An event becomes a context, and the enabled rules are checked against it
//! in the order of their ids. A rule that trips acts; the rules after it are not looked at, unless it
//! says they should be. The counter triggers that the actions fire are run through the same rules,
//! a few levels deep.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use closure_core::permission::Actor;
use closure_sdk::recent::{RecentActions, RecentLimits};
use closure_sdk::{
    Event, EventContext, Expect, Flow, GuildId, Level, ModuleError, StorageError, UserId,
    parse_module_config,
};
use serde_json::Value;

use crate::actions::{self, Applied, Deps};
use crate::config::{Config, Rule, Scope, Setup};
use crate::context::{Context, Source};
use crate::memory::Memory;
use crate::spam;
use crate::triggers::{self, Hit, Scan};

/// A counter trigger fired at this depth still runs its rules; one fired deeper is dropped.
const MAX_DEPTH: u8 = 3;

/// What the module keeps between events: the recent actions of every guild, the memory of the bot's
/// own role changes and reports, and the rules of each guild read once rather than on every event
/// (a regex is compiled when it is read).
#[derive(Debug)]
pub struct State {
    recent: RecentActions,
    loaded: Mutex<HashMap<GuildId, Arc<Loaded>>>,
    pub memory: Memory,
}

/// A guild's configuration as it was read, with its rules and counters.
#[derive(Debug)]
pub struct Loaded {
    /// The configuration it was read from; the reading is kept while this stays the same.
    source: Value,
    pub config: Config,
    pub setup: Setup,
}

impl Loaded {
    /// The rules that are valid, in the order of their ids.
    pub fn valid_rules(&self) -> impl Iterator<Item = &Rule> {
        self.setup.rules.iter().filter_map(|entry| match entry {
            crate::config::Entry::Valid(rule) => Some(rule),
            crate::config::Entry::Invalid { .. } => None,
        })
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            recent: RecentActions::new(RecentLimits::default()),
            loaded: Mutex::new(HashMap::new()),
            memory: Memory::default(),
        }
    }

    pub fn load(&self, ctx: &EventContext) -> Result<Arc<Loaded>, ModuleError> {
        let mut loaded = self.loaded.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = loaded.get(&ctx.guild_id)
            && found.source == *ctx.config()
        {
            return Ok(Arc::clone(found));
        }
        let config: Config = parse_module_config(ctx)?;
        let setup = crate::config::setup(&config, ctx.store());
        let fresh = Arc::new(Loaded {
            source: ctx.config().clone(),
            config,
            setup,
        });
        loaded.insert(ctx.guild_id, Arc::clone(&fresh));
        Ok(fresh)
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// A context waiting to be checked, and how deep in a chain of counter triggers it is.
#[derive(Debug)]
struct Work {
    context: Context,
    depth: u8,
}

/// Handles a gateway event the rules read.
pub async fn on_event(state: &State, ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Some(mut context) = Context::of_event(&event) else {
        return Ok(Flow::Continue);
    };
    // An edit with neither text nor files (a link preview, say) has nothing to check.
    if let Some(message) = &context.message
        && !context.created
        && message.content.is_empty()
        && message.attachments.is_empty()
    {
        return Ok(Flow::Continue);
    }
    // The bot's own role changes are remembered as they are made; Discord reports them too.
    let user = context.user_id();
    if let (Source::RoleChange { added, removed }, Some(user)) = (&mut context.source, user) {
        let (guild, now) = (ctx.guild_id, ctx.now());
        added.retain(|role| !state.memory.take_own_role(guild, user, *role, now));
        removed.retain(|role| !state.memory.take_own_role(guild, user, *role, now));
        if added.is_empty() && removed.is_empty() {
            return Ok(Flow::Continue);
        }
    }
    let loaded = state.load(&ctx)?;
    // The quarantine of a joining member is applied whatever the rules say, and a leaving member is
    // forgotten by it.
    let quarantine =
        crate::antiraid::member_event(&ctx, &state.memory, loaded.setup.antiraid.as_ref(), &event)
            .await;
    drain(
        state,
        &ctx,
        &loaded,
        VecDeque::from([Work { context, depth: 0 }]),
    )
    .await?;
    quarantine?;
    Ok(Flow::Continue)
}

/// Runs the contexts that counter changes made by a command fired, through the rules.
pub async fn after_command(
    state: &State,
    ctx: &EventContext,
    contexts: Vec<Context>,
) -> Result<(), ModuleError> {
    let loaded = state.load(ctx)?;
    let queue = contexts
        .into_iter()
        .map(|context| Work { context, depth: 1 })
        .collect();
    drain(state, ctx, &loaded, queue).await
}

/// Checks every queued context, and keeps going after an error: the first one is returned at the end.
async fn drain(
    state: &State,
    ctx: &EventContext,
    loaded: &Arc<Loaded>,
    mut queue: VecDeque<Work>,
) -> Result<(), ModuleError> {
    let mut first_error = None;
    while let Some(work) = queue.pop_front() {
        if work.depth > MAX_DEPTH {
            first_error.get_or_insert(ModuleError::other(
                "a counter trigger fired deeper than the chain of rules allows, and was dropped",
            ));
            continue;
        }
        if let Err(error) = process(state, ctx, loaded, work, &mut queue).await {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Checks one context against the rules in scope for it. A counter change an action makes queues
/// the triggers it fired.
async fn process(
    state: &State,
    ctx: &EventContext,
    loaded: &Loaded,
    work: Work,
    queue: &mut VecDeque<Work>,
) -> Result<(), ModuleError> {
    let Work { mut context, depth } = work;
    // The bot's own events and the server owner's are never checked, whatever the rules say.
    if let Some(user) = context.user_id()
        && (ctx.bot() == Some(user) || ctx.api.guild_owner().await? == Some(user))
    {
        return Ok(());
    }
    let now = ctx.now();
    let mut scoped = Vec::new();
    for rule in loaded.valid_rules().filter(|rule| rule.enabled) {
        if in_scope(ctx, &rule.scope, &mut context).await? {
            scoped.push(rule);
        }
    }
    let scan = Scan {
        ctx,
        recent: &state.recent,
        commit: !loaded.config.dry_run,
        created: context.created,
    };
    // Counted before any rule is checked, so that the event which trips a burst is one of it.
    if depth == 0 {
        for rule in &scoped {
            spam::record(&scan, rule, &context, now);
        }
    }
    let mut first_error = None;
    for rule in scoped {
        if on_cooldown(ctx, rule, &context).await? {
            continue;
        }
        let Some(hit) = first_hit(&scan, rule, &context) else {
            continue;
        };
        // Taken before the actions, so that a second evaluation of the same member and rule (in
        // another channel, say) finds it taken and does nothing.
        if !loaded.config.dry_run && !hit.join && !reserve_cooldown(ctx, rule, &context).await? {
            continue;
        }
        let applied = act(state, ctx, loaded, rule, &hit, &context).await;
        // The counter changes are stored already, so their triggers run even if an action failed.
        queue.extend(applied.fired.into_iter().map(|context| Work {
            context,
            depth: depth + 1,
        }));
        if let Some(error) = applied.error {
            first_error.get_or_insert(error);
        }
        if !rule.continue_after_match {
            break;
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// The first trigger of the rule that finds something, with what it found.
fn first_hit(scan: &Scan<'_>, rule: &Rule, context: &Context) -> Option<Hit> {
    (0..rule.triggers.len()).find_map(|index| triggers::check(scan, rule, index, context))
}

/// Acts on a hit. A dry run only reports it. Returns the counter triggers the actions fired.
async fn act(
    state: &State,
    ctx: &EventContext,
    loaded: &Loaded,
    rule: &Rule,
    hit: &Hit,
    context: &Context,
) -> Applied {
    if loaded.config.dry_run {
        let error = actions::report(ctx, rule, hit, context).await.err();
        return Applied {
            fired: Vec::new(),
            error,
        };
    }
    let deps = Deps {
        ctx,
        memory: &state.memory,
        counters: &loaded.setup.counters,
        config: &loaded.config,
        antiraid: loaded.setup.antiraid.as_ref(),
    };
    actions::apply(&deps, rule, hit, context).await
}

/// Whether the rule applies to the context at all: its channels, the member's roles, bots,
/// moderators unless the rule says otherwise, and the member's age when the rule asks for it.
async fn in_scope(
    ctx: &EventContext,
    scope: &Scope,
    context: &mut Context,
) -> Result<bool, ModuleError> {
    if let Some(channel) = context.channel {
        if !scope.channels.is_empty() && !scope.channels.contains(&channel) {
            return Ok(false);
        }
        if scope.exclude_channels.contains(&channel) {
            return Ok(false);
        }
    } else if !scope.channels.is_empty() {
        return Ok(false);
    }
    let age = scope.member_age;
    let Some(who) = context.user.as_mut() else {
        // Without a member, only a rule that names no member of its own applies.
        return Ok(scope.roles.is_empty() && age.min_days.is_none() && age.max_days.is_none());
    };
    if who.bot && !scope.include_bots {
        return Ok(false);
    }
    if scope.exempt_moderators || !scope.roles.is_empty() || !scope.exclude_roles.is_empty() {
        let roles = who.roles(ctx).await?.to_vec();
        if !scope.roles.is_empty() && !roles.iter().any(|role| scope.roles.contains(role)) {
            return Ok(false);
        }
        if roles.iter().any(|role| scope.exclude_roles.contains(role)) {
            return Ok(false);
        }
        if scope.exempt_moderators && is_moderator(ctx, who.id, &roles) {
            return Ok(false);
        }
    }
    if age.min_days.is_none() && age.max_days.is_none() {
        return Ok(true);
    }
    let Some(joined_at) = who.joined(ctx).await? else {
        return Ok(false);
    };
    let days = (ctx.now() - joined_at).div_euclid(86_400);
    let old_enough = age.min_days.is_none_or(|min| days >= i64::from(min));
    let young_enough = age.max_days.is_none_or(|max| days <= i64::from(max));
    Ok(old_enough && young_enough)
}

/// Whether the user reaches the moderator level through the same permissions that the commands use.
/// Administrators are not known from an event, so they count as members here.
fn is_moderator(ctx: &EventContext, user: UserId, roles: &[closure_sdk::RoleId]) -> bool {
    let actor = Actor {
        user,
        roles: roles.to_vec(),
        administrator: false,
        owner: false,
    };
    ctx.permissions().level_of(&actor) >= Level::MODERATOR
}

fn cooldown_key(rule: &Rule, context: &Context) -> String {
    let user = context.user_id().map_or(0, UserId::get);
    format!("cooldown:{}:{user}", rule.id)
}

async fn on_cooldown(
    ctx: &EventContext,
    rule: &Rule,
    context: &Context,
) -> Result<bool, ModuleError> {
    if rule.cooldown.is_none() {
        return Ok(false);
    }
    let found = ctx
        .store()
        .get::<bool>(&cooldown_key(rule, context))
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(found.is_some())
}

/// Takes the cooldown of the rule for the member, unless another evaluation has it. The write expects
/// nothing stored, so two evaluations cannot both take it. A taken cooldown stays taken when the
/// actions then fail, as a rule that matched has fired.
async fn reserve_cooldown(
    ctx: &EventContext,
    rule: &Rule,
    context: &Context,
) -> Result<bool, ModuleError> {
    let Some(cooldown) = rule.cooldown else {
        return Ok(true);
    };
    match ctx
        .store()
        .replace(
            &cooldown_key(rule, context),
            &true,
            Expect::Missing,
            Some(cooldown),
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(StorageError::Conflict) => Ok(false),
        Err(error) => Err(ModuleError::other(error.to_string())),
    }
}
