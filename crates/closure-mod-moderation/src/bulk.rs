//! Bulk punishments: a ban, a kick, a timeout or an unban of many members, in the open.
//!
//! `/bulk start` previews the count, the exclusions and the time; nothing runs until Run is pressed.
//! The work is a stored queue run a few members per batch; each member is checked again on their turn.
//! how many were done, left out and failed, and `/bulk retry` takes the failed ones again.

use std::collections::HashMap;
use std::time::Duration;

use closure_core::action::{Action, MentionPolicy};
use closure_core::error::DiscordError;
use closure_core::permission::Actor;
use closure_sdk::duration::parse as parse_duration;
use closure_sdk::punish::{Moderator, Punisher};
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{
    Args, ChannelId, CommandArgs, CommandCx, ComponentCx, Cx, EventContext, InteractionRef,
    InteractionReply, JobCx, ModuleError, RoleId, ScheduleError, Scheduler, TargetProblem,
    UserError, UserId, check_target_of,
};
use serde::{Deserialize, Serialize};
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::ButtonStyle;

use crate::{Config, delete_seconds};

pub(crate) const ROUTE: &str = "bulk";
pub(crate) const JOB: &str = "bulk";
/// The most members one operation takes: what a command option can hold.
const MAX_MEMBERS: usize = 300;
/// How many members the preview checks one by one; the rest are checked when their turn comes.
const PREVIEW_CHECKED: usize = 25;
const BATCH: usize = 5;
const PAUSE: Duration = Duration::from_secs(2);
const MAX_PROBLEMS: usize = 30;
/// How often a change of state is tried again when somebody else changed the operation first.
const TRANSITION_TRIES: usize = 5;
const KEEP: Duration = Duration::from_hours(24 * 30);
/// The operations that may run: a stop can leave one here without a job, and the guild's coming up takes it up.
const RUNNING: &str = "bulk-running";
/// How many operations the reconcile reads at a time.
const WALK: usize = 100;

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

/// Two who decide at once (a cancel and the next batch) do not both win.
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

/// Saves a batch's counters without undoing a decision made while it ran; a running operation with an empty queue is done.
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
        channel: cx.channel_id().get(),
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

/// The members still waiting. A count that cannot be read is an error: taken for zero, it would end
/// the operation as done while members are still queued.
fn left_of(count: Result<u64, closure_sdk::StorageError>) -> Result<u32, ModuleError> {
    let count = count.map_err(|error| stored(&error))?;
    u32::try_from(count)
        .map_err(|_| ModuleError::other("the queue of a bulk operation is too large"))
}

async fn waiting(ctx: &EventContext, id: i64) -> Result<u32, ModuleError> {
    left_of(ctx.store().set_count(&queue(id)).await)
}

#[cfg(test)]
mod tests {
    use super::left_of;
    use closure_sdk::StorageError;

    #[test]
    fn a_count_that_cannot_be_read_is_not_taken_for_zero_waiting() {
        assert!(matches!(left_of(Ok(3)), Ok(3)));
        assert!(left_of(Err(StorageError::Conflict)).is_err());
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one flow: read the button, check who may press it, then move the operation"
)]
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
            mark_running(ctx, id).await.map_err(|e| user_error(&e))?;
            if !transition(ctx, id, &[State::Preview], State::Running)
                .await
                .map_err(|e| user_error(&e))?
            {
                return Err(UserError::message("bulk-wrong-state"));
            }
            schedule(cx.scheduler(), id, Duration::ZERO, false)
                .await
                .map_err(|e| user_error(&e))?;
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
            if approved {
                mark_running(ctx, id).await.map_err(|e| user_error(&e))?;
            }
            // Only the first of two who decide at once decides.
            if !transition(ctx, id, &[State::Awaiting], to)
                .await
                .map_err(|e| user_error(&e))?
            {
                return Err(UserError::message("bulk-wrong-state"));
            }
            if approved {
                // What it does is checked against the one who asked: their roles, then.
                schedule(cx.scheduler(), id, Duration::ZERO, false)
                    .await
                    .map_err(|e| user_error(&e))?;
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
            mark_running(ctx, id).await.map_err(|e| user_error(&e))?;
            save(ctx, id, &operation)
                .await
                .map_err(|e| user_error(&e))?;
            schedule(cx.scheduler(), id, Duration::ZERO, false)
                .await
                .map_err(|e| user_error(&e))?;
        }
        _ => return Err(UserError::message("bulk-wrong-state")),
    }
    let left = waiting(ctx, id).await.map_err(|e| user_error(&e))?;
    let say = |key: &str, args: &Args| cx.t(key, args);
    // The preview is private to its moderator; so is what comes of pressing a button on it.
    cx.reply_private(describe(&say, id, &operation, left)).await
}

/// Takes the job of the next batch of `id`; with `if_absent` a job already waiting for it is kept.
async fn schedule(
    scheduler: &Scheduler,
    id: i64,
    after: Duration,
    if_absent: bool,
) -> Result<(), ScheduleError> {
    let job = scheduler
        .job(JOB, &serde_json::json!({ "id": id }))?
        .key(&format!("bulk-{id}"));
    let job = if if_absent { job.if_absent() } else { job };
    job.after(after).await.map(drop)
}

/// Puts `id` in the running set before it may run, so a stop before its job is taken is found again.
async fn mark_running(ctx: &EventContext, id: i64) -> Result<(), ModuleError> {
    let member =
        u64::try_from(id).map_err(|_| ModuleError::other("the bulk number is not positive"))?;
    ctx.store()
        .set_add(RUNNING, member)
        .await
        .map(drop)
        .map_err(|e| stored(&e))
}

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
    let left = waiting(ctx, args.number)
        .await
        .map_err(|e| user_error(&e))?;
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
    mark_running(ctx, args.number)
        .await
        .map_err(|e| user_error(&e))?;
    save(ctx, args.number, &operation)
        .await
        .map_err(|e| user_error(&e))?;
    schedule(cx.scheduler(), args.number, Duration::ZERO, false)
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

/// What a member's punishment came to, written before it is made so that a stop after it is finished
/// from here and not made again.
#[derive(Debug, Serialize, Deserialize)]
struct Attempt {
    /// When a timed punishment ends (unix seconds), fixed by the first try.
    ends: Option<i64>,
    done: bool,
    /// The case of the punishment, once it is recorded.
    case: Option<i64>,
}

fn attempt_key(id: i64, user: u64) -> String {
    format!("bulk-done:{id}:{user}")
}

/// The members of a batch that share one duration, with the key and end of each one's attempt.
type BanGroup = (Option<Duration>, Vec<(UserId, String, Option<i64>)>);

/// What an attempt on one member comes to before the action is taken.
enum Begun {
    /// Done before, or a timed punishment that ran out during a stop: nothing is made again.
    Finished,
    /// The target check refused the member.
    Refused(UserError),
    /// To be punished now, for `left` (None when for good), and recorded under `key`.
    Ready {
        key: String,
        ends: Option<i64>,
        left: Option<Duration>,
    },
}

/// Starts the attempt on one member: keeps the receipt of an earlier try, or writes a new one.
async fn begin(
    ctx: &EventContext,
    actor: &Actor,
    operation: &Operation,
    id: i64,
    user: UserId,
) -> Result<Begun, ModuleError> {
    let key = attempt_key(id, user.get());
    let earlier: Option<Attempt> = ctx.store().get(&key).await.map_err(|e| stored(&e))?;
    if earlier.as_ref().is_some_and(|attempt| attempt.done) {
        return Ok(Begun::Finished);
    }
    if operation.what != What::Unban
        && let Err(error) = check_target_of(ctx, actor, user).await
    {
        return Ok(Begun::Refused(error));
    }
    let now = ctx.now();
    let (ends, resumed) = if let Some(attempt) = earlier {
        (attempt.ends, true)
    } else {
        let ends = operation
            .seconds
            .map(|seconds| now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX)));
        let attempt = Attempt {
            ends,
            done: false,
            case: None,
        };
        ctx.store()
            .set_for(&key, &attempt, KEEP)
            .await
            .map_err(|e| stored(&e))?;
        (ends, false)
    };
    if resumed && ends.is_some_and(|ends| ends <= now) {
        let attempt = Attempt {
            ends,
            done: true,
            case: None,
        };
        ctx.store()
            .set_for(&key, &attempt, KEEP)
            .await
            .map_err(|e| stored(&e))?;
        return Ok(Begun::Finished);
    }
    let left =
        ends.map(|ends| Duration::from_secs(u64::try_from(ends.saturating_sub(now)).unwrap_or(0)));
    Ok(Begun::Ready { key, ends, left })
}

/// Records what the action on a member came to: the receipt of a case, or the error.
async fn settle(
    ctx: &EventContext,
    key: &str,
    ends: Option<i64>,
    made: Result<i64, UserError>,
) -> Result<Result<(), UserError>, ModuleError> {
    match made {
        Ok(number) => {
            let attempt = Attempt {
                ends,
                done: true,
                case: Some(number),
            };
            ctx.store()
                .set_for(key, &attempt, KEEP)
                .await
                .map_err(|e| stored(&e))?;
            Ok(Ok(()))
        }
        Err(error) => Ok(Err(error)),
    }
}

/// Does the action on one member once. A member done before keeps its receipt, and a timed
/// punishment that ran out during a stop is not made again.
async fn act_on(
    ctx: &EventContext,
    punisher: &Punisher<'_>,
    actor: &Actor,
    operation: &Operation,
    id: i64,
    user: UserId,
) -> Result<Result<(), UserError>, ModuleError> {
    let (key, ends, left) = match begin(ctx, actor, operation, id, user).await? {
        Begun::Finished => return Ok(Ok(())),
        Begun::Refused(error) => return Ok(Err(error)),
        Begun::Ready { key, ends, left } => (key, ends, left),
    };
    let reason = Some(operation.reason.clone());
    let made = match operation.what {
        What::Ban => punisher
            .ban(
                user,
                left,
                operation.delete_days.saturating_mul(86_400),
                reason,
            )
            .await
            .map(|done| done.case.number),
        What::Kick => punisher
            .kick(user, reason)
            .await
            .map(|done| done.case.number),
        What::Timeout => punisher
            .timeout(user, left.unwrap_or_default(), reason)
            .await
            .map(|done| done.case.number),
        What::Unban => punisher.unban(user, reason).await.map(|case| case.number),
    };
    settle(ctx, &key, ends, made).await
}

/// Bans the members of one batch in bulk requests, and returns what became of each member. The
/// members that are not banned in bulk keep their outcome from the single ban that followed.
async fn ban_batch(
    ctx: &EventContext,
    punisher: &Punisher<'_>,
    actor: &Actor,
    operation: &Operation,
    id: i64,
    members: &[u64],
) -> Result<HashMap<u64, Result<(), UserError>>, ModuleError> {
    let mut outcomes = HashMap::new();
    // One request names one duration, so the members are grouped by the time each still has.
    let mut groups: Vec<BanGroup> = Vec::new();
    for &member in members {
        let user = UserId::new(member);
        match begin(ctx, actor, operation, id, user).await? {
            Begun::Finished => {
                outcomes.insert(member, Ok(()));
            }
            Begun::Refused(error) => {
                outcomes.insert(member, Err(error));
            }
            Begun::Ready { key, ends, left } => {
                match groups
                    .iter_mut()
                    .find(|(group_left, _)| *group_left == left)
                {
                    Some((_, group)) => group.push((user, key, ends)),
                    None => groups.push((left, vec![(user, key, ends)])),
                }
            }
        }
    }
    let reason = Some(operation.reason.clone());
    for (left, group) in groups {
        let targets: Vec<UserId> = group.iter().map(|(user, _, _)| *user).collect();
        let mut pending: HashMap<UserId, (String, Option<i64>)> = group
            .into_iter()
            .map(|(user, key, ends)| (user, (key, ends)))
            .collect();
        let results = punisher
            .ban_many(
                &targets,
                left,
                operation.delete_days.saturating_mul(86_400),
                reason.clone(),
            )
            .await;
        for (user, result) in results {
            let Some((key, ends)) = pending.remove(&user) else {
                continue;
            };
            let made = result.map(|done| done.case.number);
            outcomes.insert(user.get(), settle(ctx, &key, ends, made).await?);
        }
    }
    Ok(outcomes)
}

/// Takes up the operations a stop left running: one still running gets its next batch (a job already
/// waiting for it is kept), one no longer running leaves the set.
pub(crate) async fn reconcile(ctx: &EventContext) -> Result<(), ModuleError> {
    let mut after = 0_u64;
    loop {
        let page = ctx
            .store()
            .set_after(RUNNING, after, WALK)
            .await
            .map_err(|e| stored(&e))?;
        let Some(last) = page.last().copied() else {
            return Ok(());
        };
        after = last;
        for member in page {
            let id = i64::try_from(member)
                .map_err(|_| ModuleError::other("the bulk number is too large"))?;
            if load(ctx, id)
                .await?
                .is_some_and(|operation| operation.state == State::Running)
            {
                schedule(ctx.scheduler(), id, Duration::ZERO, true)
                    .await
                    .map_err(|e| ModuleError::other(e.to_string()))?;
            } else {
                ctx.store()
                    .set_remove(RUNNING, member)
                    .await
                    .map_err(|e| stored(&e))?;
            }
        }
    }
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
    // The same settings as the single commands: a timeout by role is a role here too.
    let settings = crate::punish_settings(ctx, &config)
        .await
        .map_err(|error| ModuleError::other(format!("{error:?}")))?;
    let moderator = Moderator {
        id: actor.user,
        name: operation.by_name.clone(),
    };
    let punisher = Punisher::new(ctx, moderator, settings);
    // A member leaves the queue only once the action on them has an outcome; the receipt of a member
    // already done keeps a stop in between from punishing them twice.
    let batch = ctx
        .store()
        .set_sample(&queue(id), BATCH)
        .await
        .map_err(|e| stored(&e))?;
    // Bans of a batch go out in bulk requests, unless the operation was stopped before the batch.
    let running = load(ctx, id)
        .await?
        .is_some_and(|current| current.state == State::Running);
    let mut outcomes = if operation.what == What::Ban && batch.len() > 1 && running {
        ban_batch(ctx, &punisher, &actor, &operation, id, &batch).await?
    } else {
        HashMap::new()
    };
    let mut pause = PAUSE;
    for member in batch {
        // A cancel is heard between two members, not only between two batches.
        if load(ctx, id)
            .await?
            .is_none_or(|current| current.state != State::Running)
        {
            break;
        }
        let result = match outcomes.remove(&member) {
            Some(result) => result,
            None => act_on(ctx, &punisher, &actor, &operation, id, UserId::new(member)).await?,
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
    let left = waiting(ctx, id).await?;
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
    let left = waiting(cx.ctx(), id).await.map_err(|e| user_error(&e))?;
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
pub(crate) async fn request_ban<I: InteractionRef + 'static>(
    cx: &Cx<I>,
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
        channel: cx.channel_id().get(),
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
