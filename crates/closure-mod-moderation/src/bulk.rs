//! Bulk punishments: a ban, a kick, a timeout or an unban of many members, in the open.
//!
//! `/bulk start` shows what it would do first: how many, who is left out and why (the first
//! members are checked), and about how long it takes. Nothing is done until somebody presses Run.
//! The work is a queue (a set of the store) that a job takes from a few members at a time, so it
//! survives a restart, can be cancelled between two batches, and is slow enough for Discord's
//! limits. Each member is checked again when their turn comes: the permission of the moderator
//! at the time they pressed Run, the roles, a bot or the owner. At the end the result is posted:
//! how many were done, left out and failed, and `/bulk retry` takes the failed ones again.

use std::time::Duration;

use closure_core::action::{Action, MentionPolicy};
use closure_core::error::DiscordError;
use closure_core::permission::Actor;
use closure_sdk::duration::parse as parse_duration;
use closure_sdk::punish::{Moderator, Punisher, Settings};
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{
    Args, ChannelId, CommandArgs, CommandCx, ComponentCx, EventContext, InteractionReply, JobCx,
    ModuleError, RoleId, TargetProblem, UserError, UserId, check_target_of,
};
use serde::{Deserialize, Serialize};
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::ButtonStyle;

use crate::{Config, delete_seconds};

/// The route of the buttons, and the name of the job.
pub(crate) const ROUTE: &str = "bulk";
pub(crate) const JOB: &str = "bulk";
/// The most members one operation takes: what a command option can hold.
const MAX_MEMBERS: usize = 300;
/// How many members the preview checks one by one; the rest are checked when their turn comes.
const PREVIEW_CHECKED: usize = 25;
/// How many members one run of the job does.
const BATCH: usize = 5;
/// The pause between two batches.
const PAUSE: Duration = Duration::from_secs(2);
/// How many problems an operation remembers by name.
const MAX_PROBLEMS: usize = 30;
/// How often a change of state is tried again when somebody else changed the operation first.
const TRANSITION_TRIES: usize = 5;
/// How long an operation is kept after its last change.
const KEEP: Duration = Duration::from_hours(24 * 30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum What {
    Ban,
    Kick,
    Timeout,
    Unban,
}

impl What {
    fn named(text: &str) -> Option<Self> {
        match text.trim().to_lowercase().as_str() {
            "ban" => Some(Self::Ban),
            "kick" => Some(Self::Kick),
            "timeout" => Some(Self::Timeout),
            "unban" => Some(Self::Unban),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ban => "ban",
            Self::Kick => "kick",
            Self::Timeout => "timeout",
            Self::Unban => "unban",
        }
    }

    /// The permission the single command of the same name asks.
    fn permission(self) -> &'static str {
        match self {
            Self::Ban | Self::Unban => "moderation.ban",
            Self::Kick => "moderation.kick",
            Self::Timeout => "moderation.timeout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    /// Shown, not started.
    Preview,
    /// Asked for, and waiting for a second moderator to approve it.
    Awaiting,
    Running,
    Cancelled,
    Done,
}

impl State {
    fn key(self) -> &'static str {
        match self {
            Self::Preview => "bulk-state-preview",
            Self::Awaiting => "bulk-state-awaiting",
            Self::Running => "bulk-state-running",
            Self::Cancelled => "bulk-state-cancelled",
            Self::Done => "bulk-state-done",
        }
    }
}

/// Why a member was not done.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Problem {
    user: u64,
    why: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Operation {
    what: What,
    state: State,
    /// Who started it, and what they were then: the checks are theirs.
    by: u64,
    by_name: String,
    by_roles: Vec<u64>,
    by_owner: bool,
    reason: String,
    seconds: Option<u64>,
    delete_days: u32,
    /// Where the result is posted.
    channel: u64,
    total: u32,
    done: u32,
    skipped: u32,
    failed: u32,
    problems: Vec<Problem>,
    /// When it was asked for (unix seconds): an approval is not good for ever.
    #[serde(default)]
    created: i64,
}

fn key(id: i64) -> String {
    format!("bulk:{id}")
}

/// The members still to do, and the members that failed and can be done again.
fn queue(id: i64) -> String {
    format!("bulk-{id}")
}

fn failed_set(id: i64) -> String {
    format!("bulk-{id}-failed")
}

fn stored(error: &closure_sdk::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn user_error(error: &impl ToString) -> UserError {
    UserError::from(ModuleError::other(error.to_string()))
}

async fn load(ctx: &EventContext, id: i64) -> Result<Option<Operation>, ModuleError> {
    ctx.store().get(&key(id)).await.map_err(|e| stored(&e))
}

async fn save(ctx: &EventContext, id: i64, operation: &Operation) -> Result<(), ModuleError> {
    ctx.store()
        .set_for(&key(id), operation, KEEP)
        .await
        .map_err(|e| stored(&e))
}

/// Moves an operation from one of the states `from` to `to`, against the revision it was read at, so
/// two who decide at once (a cancel and the next batch, an approval and a denial) do not both win.
/// Whether it moved.
async fn transition(
    ctx: &EventContext,
    id: i64,
    from: &[State],
    to: State,
) -> Result<bool, ModuleError> {
    for _ in 0..TRANSITION_TRIES {
        let Some(entry) = ctx.store().entry(&key(id)).await.map_err(|e| stored(&e))? else {
            return Ok(false);
        };
        let mut operation: Operation = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if !from.contains(&operation.state) {
            return Ok(false);
        }
        operation.state = to;
        match ctx
            .store()
            .replace(
                &key(id),
                &operation,
                closure_core::storage::Expect::Revision(entry.revision),
                Some(KEEP),
            )
            .await
        {
            Ok(_) => return Ok(true),
            Err(closure_sdk::StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "the bulk operation is changed too often",
    ))
}

/// Saves what a batch did (the counters) without taking back a decision made while it ran: the
/// state is whatever it is now, except that a running operation whose queue is empty is done.
/// Returns the state it was left in.
async fn record_progress(
    ctx: &EventContext,
    id: i64,
    ours: &Operation,
    finished: bool,
) -> Result<State, ModuleError> {
    for _ in 0..TRANSITION_TRIES {
        let Some(entry) = ctx.store().entry(&key(id)).await.map_err(|e| stored(&e))? else {
            return Ok(ours.state);
        };
        let current: Operation = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let mut merged = ours.clone();
        merged.state = match current.state {
            State::Running if finished => State::Done,
            other => other,
        };
        match ctx
            .store()
            .replace(
                &key(id),
                &merged,
                closure_core::storage::Expect::Revision(entry.revision),
                Some(KEEP),
            )
            .await
        {
            Ok(_) => return Ok(merged.state),
            Err(closure_sdk::StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "the bulk operation is changed too often",
    ))
}

/// A member that is not done, as the one who started the operation is told.
fn problem_key(error: &UserError) -> &'static str {
    match error {
        UserError::Target(TargetProblem::SelfTarget) => "bulk-why-self",
        UserError::Target(TargetProblem::Owner) => "bulk-why-owner",
        UserError::Target(TargetProblem::Bot) => "bulk-why-bot",
        UserError::Target(TargetProblem::Higher) => "bulk-why-higher",
        UserError::Module(ModuleError::Discord(
            DiscordError::Forbidden(_) | DiscordError::HierarchyViolation(_),
        )) => "bulk-why-forbidden",
        UserError::Module(ModuleError::Discord(_)) => "bulk-why-discord",
        _ => "bulk-why-failed",
    }
}

fn actor_of(operation: &Operation) -> Actor {
    Actor {
        user: UserId::new(operation.by),
        roles: operation.by_roles.iter().map(|r| RoleId::new(*r)).collect(),
        administrator: false,
        owner: operation.by_owner,
    }
}

#[derive(CommandArgs)]
pub(crate) struct Start {
    /// What to do: ban, kick, timeout or unban
    #[arg(max_length = 16)]
    action: String,
    /// User IDs or mentions separated by spaces or commas
    users: String,
    /// Why
    #[arg(max_length = 512)]
    reason: String,
    /// How long a timeout lasts, such as 1h or 3d; a ban lasts for good when it is left out
    #[arg(max_length = 32)]
    duration: Option<String>,
    /// Days of messages a ban deletes, 0 to 7
    #[arg(min = 0, max = 7)]
    delete_days: Option<i64>,
}

#[derive(CommandArgs)]
pub(crate) struct OfOperation {
    /// The number of the operation
    number: i64,
}

/// The buttons of a preview: Run and Cancel, in the words of the one who reads.
fn buttons(
    module: &closure_core::module::ModuleId,
    by: UserId,
    id: i64,
    run: String,
    cancel: String,
) -> Vec<Component> {
    let button = |route: &str, label: String, style| {
        ui::button(
            style,
            label,
            CustomId::new(ROUTE)
                .only_for(by)
                .data(format!("{route}:{id}"))
                .encode(module),
        )
    };
    vec![ui::row(vec![
        button("run", run, ButtonStyle::Success),
        button("cancel", cancel, ButtonStyle::Danger),
    ])]
}

/// Parses what the moderator asked for, checks the first members, and shows it all.
#[allow(
    clippy::too_many_lines,
    reason = "one flow: read the request, check the first members, queue the rest, show it"
)]
pub(crate) async fn start(cx: CommandCx, args: Start) -> Result<(), UserError> {
    let Some(what) = What::named(&args.action) else {
        return Err(UserError::message("bulk-action-unknown"));
    };
    cx.require(what.permission()).await?;
    let mut users: Vec<UserId> = Vec::new();
    for token in args
        .users
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|s| !s.is_empty())
    {
        if let Some(user) = parse_user(token)
            && !users.contains(&user)
        {
            users.push(user);
        }
    }
    if users.is_empty() || args.reason.trim().is_empty() {
        return Err(UserError::message("bulk-missing"));
    }
    if users.len() > MAX_MEMBERS {
        return Err(UserError::message("bulk-too-many"));
    }
    let seconds = match (&args.duration, what) {
        (Some(text), What::Timeout | What::Ban) => Some(
            parse_duration(text)
                .map_err(|_| UserError::message("bulk-duration-bad"))?
                .as_secs(),
        ),
        (None, What::Timeout) => return Err(UserError::message("bulk-timeout-needs-duration")),
        _ => None,
    };
    let config: Config = cx.config()?;
    let days = args
        .delete_days
        .unwrap_or(i64::from(config.ban_delete_days));
    delete_seconds(days)?;
    cx.defer(true).await?;
    let actor = cx.actor().await;
    let ctx = cx.ctx();
    let id = ctx
        .store()
        .score_add("bulk-seq", 0_u64, 1)
        .await
        .map_err(|e| user_error(&e))?;
    let mut operation = Operation {
        what,
        state: State::Preview,
        by: actor.user.get(),
        by_name: cx.user().name.clone(),
        by_roles: actor.roles.iter().map(|r| r.get()).collect(),
        by_owner: actor.owner,
        reason: args.reason.clone(),
        seconds,
        delete_days: u32::try_from(days).unwrap_or(0),
        channel: cx.invocation().channel_id.get(),
        total: u32::try_from(users.len()).unwrap_or(u32::MAX),
        done: 0,
        skipped: 0,
        failed: 0,
        problems: Vec::new(),
        created: cx.now(),
    };
    // The first members are checked now, so the moderator sees who cannot be done; the others
    // are checked as their turn comes.
    let mut waiting = 0_u32;
    for (at, user) in users.iter().enumerate() {
        let verdict = if at < PREVIEW_CHECKED && what != What::Unban {
            check_target_of(ctx, &actor, *user).await.err()
        } else {
            None
        };
        if let Some(error) = verdict {
            operation.skipped += 1;
            remember(
                &mut operation,
                user.get(),
                cx.t(problem_key(&error), &Args::new()),
            );
        } else {
            waiting += 1;
            ctx.store()
                .set_add(&queue(id), user.get())
                .await
                .map_err(|e| user_error(&e))?;
        }
    }
    save(ctx, id, &operation)
        .await
        .map_err(|e| user_error(&e))?;
    let say = |key: &str, args: &Args| cx.t(key, args);
    let text = describe(&say, id, &operation, waiting);
    cx.reply_with(InteractionReply {
        content: Some(text),
        embeds: Vec::new(),
        ephemeral: true,
        components: if waiting > 0 {
            buttons(
                cx.module(),
                actor.user,
                id,
                cx.t("bulk-run", &Args::new()),
                cx.t("bulk-cancel", &Args::new()),
            )
        } else {
            Vec::new()
        },
    })
    .await
}

fn remember(operation: &mut Operation, user: u64, why: String) {
    if operation.problems.len() < MAX_PROBLEMS {
        operation.problems.push(Problem { user, why });
    }
}

/// What an operation is and where it stands, in words. `say` is the translator of whoever reads.
fn describe(
    say: &dyn Fn(&str, &Args) -> String,
    id: i64,
    operation: &Operation,
    waiting: u32,
) -> String {
    let mut text = say(
        "bulk-header",
        &Args::new()
            .with("number", id)
            .with("action", operation.what.name().to_owned())
            .with("state", say(operation.state.key(), &Args::new())),
    );
    text.push('\n');
    text.push_str(&say(
        "bulk-counts",
        &Args::new()
            .with("total", u64::from(operation.total))
            .with("done", u64::from(operation.done))
            .with("waiting", u64::from(waiting))
            .with("skipped", u64::from(operation.skipped))
            .with("failed", u64::from(operation.failed)),
    ));
    if operation.state == State::Preview && waiting > 0 {
        // About how long: a batch every few seconds, and what each call costs.
        let batches = u64::from(waiting).div_ceil(BATCH as u64);
        let minutes = (batches * (PAUSE.as_secs() + BATCH as u64)).div_ceil(60);
        text.push('\n');
        text.push_str(&say("bulk-estimate", &Args::new().with("minutes", minutes)));
    }
    for problem in &operation.problems {
        text.push('\n');
        text.push_str(&say(
            "bulk-problem",
            &Args::new()
                .with("user", format!("<@{}>", problem.user))
                .with("why", problem.why.clone()),
        ));
    }
    closure_sdk::embed::truncate(&text, crate::MAX_CHARS)
}

/// How many members still wait.
async fn waiting(ctx: &EventContext, id: i64) -> u32 {
    ctx.store()
        .set_count(&queue(id))
        .await
        .ok()
        .and_then(|count| u32::try_from(count).ok())
        .unwrap_or(0)
}

/// The press of Run, Cancel or Retry.
pub(crate) async fn press(cx: ComponentCx) -> Result<(), UserError> {
    let data = cx.data();
    let Some((action, id)) = data
        .split_once(':')
        .and_then(|(action, id)| Some((action.to_owned(), id.parse::<i64>().ok()?)))
    else {
        return Err(UserError::message("bulk-not-found"));
    };
    let ctx = cx.ctx();
    let Some(mut operation) = load(ctx, id).await.map_err(|e| user_error(&e))? else {
        return Err(UserError::message("bulk-not-found"));
    };
    // The one who started it is who may press Run, Cancel and Retry: the checks are theirs (the
    // button says so too). Approving and denying are for somebody else.
    let approving = matches!(action.as_str(), "approve" | "deny");
    if !approving && operation.by != cx.user().id.get() {
        return Err(UserError::message("bulk-not-yours"));
    }
    match (action.as_str(), operation.state) {
        ("run", State::Preview) => {
            // The permission is asked again at the moment of the press.
            cx.require(operation.what.permission()).await?;
            let config: Config = cx.config()?;
            if needs_approval(&cx, &config, &operation).await {
                return ask_for_approval(&cx, &config, id, &mut operation).await;
            }
            if !transition(ctx, id, &[State::Preview], State::Running)
                .await
                .map_err(|e| user_error(&e))?
            {
                return Err(UserError::message("bulk-wrong-state"));
            }
            schedule(&cx, id, Duration::ZERO).await?;
        }
        ("approve" | "deny", State::Awaiting) => {
            cx.require("moderation.approve").await?;
            if operation.by == cx.user().id.get() {
                return Err(UserError::message("bulk-approve-own"));
            }
            if cx.now() - operation.created > APPROVAL_VALID.as_secs().cast_signed() {
                transition(ctx, id, &[State::Awaiting], State::Cancelled)
                    .await
                    .map_err(|e| user_error(&e))?;
                return Err(UserError::message("bulk-approval-expired"));
            }
            let approved = action == "approve";
            let to = if approved {
                State::Running
            } else {
                State::Cancelled
            };
            // Only the first of two who decide at once decides.
            if !transition(ctx, id, &[State::Awaiting], to)
                .await
                .map_err(|e| user_error(&e))?
            {
                return Err(UserError::message("bulk-wrong-state"));
            }
            if approved {
                // What it does is checked against the one who asked: their roles, then.
                schedule(&cx, id, Duration::ZERO).await?;
            }
            return cx
                .reply_private_t(
                    if approved {
                        "bulk-approved-by"
                    } else {
                        "bulk-denied-by"
                    },
                    &Args::new()
                        .with("number", id)
                        .with("user", cx.user().name.clone()),
                )
                .await;
        }
        ("cancel", State::Preview | State::Awaiting | State::Running) => {
            transition(
                ctx,
                id,
                &[State::Preview, State::Awaiting, State::Running],
                State::Cancelled,
            )
            .await
            .map_err(|e| user_error(&e))?;
        }
        ("retry", State::Done | State::Cancelled) => {
            cx.require(operation.what.permission()).await?;
            let again = retry_failed(ctx, id, &mut operation)
                .await
                .map_err(|e| user_error(&e))?;
            if again == 0 {
                return Err(UserError::message("bulk-nothing-to-retry"));
            }
            save(ctx, id, &operation)
                .await
                .map_err(|e| user_error(&e))?;
            schedule(&cx, id, Duration::ZERO).await?;
        }
        _ => return Err(UserError::message("bulk-wrong-state")),
    }
    let left = waiting(ctx, id).await;
    let say = |key: &str, args: &Args| cx.t(key, args);
    // The preview is private to its moderator; so is what comes of pressing a button on it.
    cx.reply_private(describe(&say, id, &operation, left)).await
}

async fn schedule(cx: &ComponentCx, id: i64, after: Duration) -> Result<(), UserError> {
    cx.scheduler()
        .job(JOB, &serde_json::json!({ "id": id }))
        .map_err(|e| user_error(&e))?
        .key(&format!("bulk-{id}"))
        .after(after)
        .await
        .map_err(|e| user_error(&e))
}

/// Puts the members that failed back in the queue; how many.
async fn retry_failed(
    ctx: &EventContext,
    id: i64,
    operation: &mut Operation,
) -> Result<usize, ModuleError> {
    let mut again = 0;
    let mut after = 0_u64;
    loop {
        // Each is in the queue before it is out of the failed set: a stop in between loses none.
        let batch = ctx
            .store()
            .set_after(&failed_set(id), after, 100)
            .await
            .map_err(|e| stored(&e))?;
        let Some(last) = batch.last().copied() else {
            break;
        };
        after = last;
        for user in batch {
            ctx.store()
                .set_add(&queue(id), user)
                .await
                .map_err(|e| stored(&e))?;
            ctx.store()
                .set_remove(&failed_set(id), user)
                .await
                .map_err(|e| stored(&e))?;
            again += 1;
        }
    }
    operation.failed = 0;
    operation.problems.clear();
    operation.state = State::Running;
    Ok(again)
}

/// `/bulk status`: where an operation stands.
pub(crate) async fn status(cx: CommandCx, args: OfOperation) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let Some(operation) = load(ctx, args.number).await.map_err(|e| user_error(&e))? else {
        return Err(UserError::message("bulk-not-found"));
    };
    let left = waiting(ctx, args.number).await;
    let say = |key: &str, args: &Args| cx.t(key, args);
    cx.reply_private(describe(&say, args.number, &operation, left))
        .await
}

/// `/bulk cancel`: stops an operation between two batches.
pub(crate) async fn cancel(cx: CommandCx, args: OfOperation) -> Result<(), UserError> {
    let ctx = cx.ctx();
    if load(ctx, args.number)
        .await
        .map_err(|e| user_error(&e))?
        .is_none()
    {
        return Err(UserError::message("bulk-not-found"));
    }
    if !transition(
        ctx,
        args.number,
        &[State::Preview, State::Awaiting, State::Running],
        State::Cancelled,
    )
    .await
    .map_err(|e| user_error(&e))?
    {
        return Err(UserError::message("bulk-wrong-state"));
    }
    cx.say_private_t(
        closure_sdk::Tone::Success,
        "bulk-cancelled",
        &Args::new().with("number", args.number),
    )
    .await
}

/// `/bulk retry`: takes the members that failed again.
pub(crate) async fn retry(cx: CommandCx, args: OfOperation) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let Some(mut operation) = load(ctx, args.number).await.map_err(|e| user_error(&e))? else {
        return Err(UserError::message("bulk-not-found"));
    };
    if !matches!(operation.state, State::Done | State::Cancelled) {
        return Err(UserError::message("bulk-wrong-state"));
    }
    cx.require(operation.what.permission()).await?;
    let again = retry_failed(ctx, args.number, &mut operation)
        .await
        .map_err(|e| user_error(&e))?;
    if again == 0 {
        return Err(UserError::message("bulk-nothing-to-retry"));
    }
    save(ctx, args.number, &operation)
        .await
        .map_err(|e| user_error(&e))?;
    cx.scheduler()
        .job(JOB, &serde_json::json!({ "id": args.number }))
        .map_err(|e| user_error(&e))?
        .key(&format!("bulk-{}", args.number))
        .after(Duration::ZERO)
        .await
        .map_err(|e| user_error(&e))?;
    cx.say_private_t(
        closure_sdk::Tone::Success,
        "bulk-retrying",
        &Args::new()
            .with("number", args.number)
            .with("count", again as u64),
    )
    .await
}

#[derive(Deserialize)]
struct Step {
    id: i64,
}

/// Does a few members of an operation, and schedules the next batch, or ends it.
#[allow(
    clippy::too_many_lines,
    reason = "one flow: take a batch, act on each member, record, schedule the next"
)]
pub(crate) async fn step(cx: JobCx) -> Result<(), ModuleError> {
    let Step { id } = cx.payload()?;
    let ctx = cx.ctx();
    let Some(mut operation) = load(ctx, id).await? else {
        return Ok(());
    };
    if operation.state != State::Running {
        return Ok(());
    }
    let config: Config = closure_sdk::parse_module_config(ctx)?;
    let actor = actor_of(&operation);
    let settings = Settings {
        dm: config.dm_on_action,
        ..Settings::default()
    };
    let moderator = Moderator {
        id: actor.user,
        name: operation.by_name.clone(),
    };
    let punisher = Punisher::new(ctx, moderator, settings);
    // A member leaves the queue only once the action on them has an outcome, so a stop in the middle
    // leaves the rest, and the one being done, to be done again (the punishments finish themselves
    // from their intents, so doing one twice is safe).
    let batch = ctx
        .store()
        .set_sample(&queue(id), BATCH)
        .await
        .map_err(|e| stored(&e))?;
    let mut pause = PAUSE;
    for member in batch {
        // A cancel is heard between two members, not only between two batches.
        if load(ctx, id)
            .await?
            .is_none_or(|current| current.state != State::Running)
        {
            break;
        }
        let user = UserId::new(member);
        let checked = if operation.what == What::Unban {
            Ok(())
        } else {
            check_target_of(ctx, &actor, user).await
        };
        let reason = Some(operation.reason.clone());
        let result = match checked {
            Err(error) => Err(error),
            Ok(()) => match operation.what {
                What::Ban => punisher
                    .ban(
                        user,
                        operation.seconds.map(Duration::from_secs),
                        operation.delete_days.saturating_mul(86_400),
                        reason,
                    )
                    .await
                    .map(drop),
                What::Kick => punisher.kick(user, reason).await.map(drop),
                What::Timeout => punisher
                    .timeout(
                        user,
                        Duration::from_secs(operation.seconds.unwrap_or(0)),
                        reason,
                    )
                    .await
                    .map(drop),
                What::Unban => punisher.unban(user, reason).await.map(drop),
            },
        };
        match result {
            Ok(()) => {}
            Err(error) => {
                // A busy Discord is not a failure of the member: they wait for the next batch,
                // and stay in the queue.
                if let UserError::Module(ModuleError::Discord(DiscordError::RateLimited {
                    retry_after,
                })) = &error
                {
                    pause = retry_after.unwrap_or(PAUSE).max(PAUSE);
                    continue;
                }
                let why = ctx.t_in(ctx.locale, problem_key(&error), &Args::new());
                if matches!(error, UserError::Target(_)) {
                    operation.skipped += 1;
                } else {
                    operation.failed += 1;
                    ctx.store()
                        .set_add(&failed_set(id), member)
                        .await
                        .map_err(|e| stored(&e))?;
                }
                remember(&mut operation, member, why);
            }
        }
        ctx.store()
            .set_remove(&queue(id), member)
            .await
            .map_err(|e| stored(&e))?;
    }
    let left = waiting(ctx, id).await;
    // What was done is what is neither waiting, nor failed, nor left out: counted from the queue, so
    // a stop that lost a count of this job does not leave the total and the result disagreeing.
    operation.done = operation
        .total
        .saturating_sub(left)
        .saturating_sub(operation.failed)
        .saturating_sub(operation.skipped);
    let state = record_progress(ctx, id, &operation, left == 0).await?;
    match state {
        State::Done => {
            operation.state = State::Done;
            announce(ctx, id, &operation).await
        }
        State::Running => cx
            .scheduler()
            .job(JOB, &serde_json::json!({ "id": id }))
            .map_err(|e| ModuleError::other(e.to_string()))?
            .key(&format!("bulk-{id}"))
            .after(pause)
            .await
            .map_err(|e| ModuleError::other(e.to_string())),
        // Cancelled while the batch ran: nothing more is scheduled.
        _ => Ok(()),
    }
}

/// Posts the result where the operation was started.
async fn announce(ctx: &EventContext, id: i64, operation: &Operation) -> Result<(), ModuleError> {
    let say = |key: &str, args: &Args| ctx.t_in(ctx.locale, key, args);
    let text = describe(&say, id, operation, 0);
    let sent = ctx
        .api
        .execute(Action::SendMessage {
            channel_id: ChannelId::new(operation.channel),
            content: Some(text),
            embeds: Vec::new(),
            reply_to: None,
            mentions: MentionPolicy::None,
        })
        .await;
    if let Err(error) = sent {
        tracing::debug!(?error, "the result of a bulk operation was not posted");
    }
    Ok(())
}

/// How long an approval can be given after it was asked for.
const APPROVAL_VALID: Duration = Duration::from_hours(24);

/// Whether this operation waits for a second moderator: it is big enough, or it is a ban for good,
/// and the one who asked cannot approve it themselves.
async fn needs_approval(cx: &ComponentCx, config: &Config, operation: &Operation) -> bool {
    let big = config.approval_bulk_from > 0 && operation.total >= config.approval_bulk_from;
    let for_good =
        config.approval_permanent_ban && operation.what == What::Ban && operation.seconds.is_none();
    if !(big || for_good) {
        return false;
    }
    // Who may approve may do it without asking another.
    cx.require("moderation.approve").await.is_err()
}

/// Puts the request in the channel for approvals, with Approve and Deny for the others.
async fn ask_for_approval(
    cx: &ComponentCx,
    config: &Config,
    id: i64,
    operation: &mut Operation,
) -> Result<(), UserError> {
    let Some(channel) = config.approval_target() else {
        return Err(UserError::message("bulk-approval-no-channel"));
    };
    operation.state = State::Awaiting;
    save(cx.ctx(), id, operation)
        .await
        .map_err(|e| user_error(&e))?;
    let left = waiting(cx.ctx(), id).await;
    let say = |key: &str, args: &Args| cx.t(key, args);
    let mut text = describe(&say, id, operation, left);
    text.push('\n');
    text.push_str(
        &cx.t(
            "bulk-approval-asked",
            &Args::new()
                .with("user", format!("<@{}>", operation.by))
                .with(
                    "reason",
                    closure_sdk::embed::truncate(&operation.reason, 200),
                ),
        ),
    );
    post_request(
        cx.api(),
        cx.module(),
        channel,
        id,
        text,
        cx.t("bulk-approve", &Args::new()),
        cx.t("bulk-deny", &Args::new()),
    )
    .await?;
    cx.reply_private_t("bulk-awaiting-approval", &Args::new().with("number", id))
        .await
}

/// Posts the request, with Approve and Deny for the others, in the channel for approvals.
async fn post_request(
    api: &closure_core::client::ApiClient,
    module: &closure_core::module::ModuleId,
    channel: ChannelId,
    id: i64,
    text: String,
    approve: String,
    deny: String,
) -> Result<(), DiscordError> {
    let button = |route: &str, label: String, style| {
        ui::button(
            style,
            label,
            CustomId::new(ROUTE)
                .data(format!("{route}:{id}"))
                .encode(module),
        )
    };
    let components = vec![ui::row(vec![
        button("approve", approve, ButtonStyle::Success),
        button("deny", deny, ButtonStyle::Danger),
    ])];
    api.execute(Action::PostMessage {
        channel_id: channel,
        message_id: None,
        reply_to: None,
        content: Some(text),
        embeds: Vec::new(),
        components,
        files: Vec::new(),
    })
    .await
    .map(drop)
}

/// A ban for good that needs a second moderator: asked for in the channel for approvals, and done
/// by the bulk machinery once somebody else approves it.
pub(crate) async fn request_ban(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    reason: String,
    days: i64,
) -> Result<(), UserError> {
    let Some(channel) = config.approval_target() else {
        return Err(UserError::message("bulk-approval-no-channel"));
    };
    delete_seconds(days)?;
    let actor = cx.actor().await;
    let ctx = cx.ctx();
    let id = ctx
        .store()
        .score_add("bulk-seq", 0_u64, 1)
        .await
        .map_err(|e| user_error(&e))?;
    let operation = Operation {
        what: What::Ban,
        state: State::Awaiting,
        by: actor.user.get(),
        by_name: cx.user().name.clone(),
        by_roles: actor.roles.iter().map(|r| r.get()).collect(),
        by_owner: actor.owner,
        reason: reason.clone(),
        seconds: None,
        delete_days: u32::try_from(days).unwrap_or(0),
        channel: cx.invocation().channel_id.get(),
        total: 1,
        done: 0,
        skipped: 0,
        failed: 0,
        problems: Vec::new(),
        created: cx.now(),
    };
    ctx.store()
        .set_add(&queue(id), user.get())
        .await
        .map_err(|e| user_error(&e))?;
    save(ctx, id, &operation)
        .await
        .map_err(|e| user_error(&e))?;
    let say = |key: &str, args: &Args| cx.t(key, args);
    let mut text = describe(&say, id, &operation, 1);
    text.push('\n');
    text.push_str(
        &cx.t(
            "bulk-approval-asked",
            &Args::new()
                .with("user", format!("<@{}>", operation.by))
                .with("reason", closure_sdk::embed::truncate(&reason, 200)),
        ),
    );
    text.push('\n');
    text.push_str(&cx.t(
        "bulk-approval-target",
        &Args::new().with("user", format!("<@{user}>")),
    ));
    post_request(
        cx.api(),
        cx.module(),
        channel,
        id,
        text,
        cx.t("bulk-approve", &Args::new()),
        cx.t("bulk-deny", &Args::new()),
    )
    .await?;
    cx.say_private_t(
        closure_sdk::Tone::Success,
        "bulk-awaiting-approval",
        &Args::new().with("number", id),
    )
    .await
}

/// A user from an id or a mention.
fn parse_user(token: &str) -> Option<UserId> {
    let value = if let Some(value) = token.strip_prefix("<@") {
        value
            .strip_suffix('>')?
            .strip_prefix('!')
            .unwrap_or(value.strip_suffix('>')?)
    } else {
        token
    };
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse::<u64>().ok().and_then(UserId::new_checked)
}
