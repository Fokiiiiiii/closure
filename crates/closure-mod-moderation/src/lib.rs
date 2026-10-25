//! The `moderation` module: warnings, notes and the cases of a server.
//!
//! Written with the SDK only. It keeps no data of its own: every warning and note is a case of
//! the SDK's cases, numbered per server and shared by the modules. A warning counts until it is
//! revoked or its expiry is due, so nothing has to run later to expire one.

use std::time::Duration;

use closure_sdk::duration::{TimeStyle, parse as parse_duration, timestamp};
use closure_sdk::embed::{escape, truncate};
use closure_sdk::modlog;
use closure_sdk::{
    Args, Capability, Case, CaseError, CaseKind, Cases, Command, CommandArgs, CommandCx, Cooldown,
    CoreError, DirectMessage, DiscordError, InteractionReply, JobCx, Level, Locale, ModuleBuilder,
    ModuleError, NewCase, PermissionSpec, Reason, ScheduleError, SdkModule, Sent, UserError,
    UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

mod restrict;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const DAY: i64 = 86_400;
/// Discord's longest timeout, 28 days in seconds.
const MAX_TIMEOUT_SECONDS: u32 = 28 * 86_400;
/// The most messages a ban deletes, 7 days in seconds. The argument's bounds are only a hint to
/// Discord, so the module checks them too.
const MAX_BAN_DELETE_SECONDS: u32 = 7 * 86_400;
/// How many active warnings `/warnings` lists, the newest first.
const SHOWN: usize = 10;
/// How many cases `/case list` lists.
const LISTED: usize = 10;
/// How many cases a page of [`Cases::for_target`] holds at most.
const PAGE: usize = 25;
/// How many cases back an active warning is looked for. shortcut: a member with more cases
/// than this has the older warnings left out of the count, upgrade with an index on the target.
const SCAN: usize = 100;
/// How many characters of a reason a list shows.
const REASON_SHOWN: usize = 80;
/// Discord's limit of a message is 2000 characters; `/warnings` stays below it.
const MAX_CHARS: usize = 1900;

/// The module's section of a guild's configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Config {
    /// After how many days a warning expires.
    warn_expire_days: u32,
    /// Whether the warned member gets a direct message.
    dm_on_action: bool,
    /// Whether a warning needs a reason.
    require_reason: bool,
    /// How many days of messages a ban deletes.
    ban_delete_days: u32,
    /// What a warning brings about once a member reaches a number of active warnings.
    escalation: Vec<Rule>,
    /// How a timeout is given: Discord's timeout, or a role that a timer takes off again.
    timeout_method: restrict::Method,
    /// The id of the role a timeout gives, when `timeout_method` is `role`.
    timeout_role: Option<String>,
    /// How a voice mute is given: Discord's voice mute, or a role that a timer takes off again.
    voice_mute_method: restrict::Method,
    /// The id of the role a voice mute gives, when `voice_mute_method` is `role`.
    voice_mute_role: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            warn_expire_days: 30,
            dm_on_action: true,
            require_reason: false,
            ban_delete_days: 0,
            escalation: Vec::new(),
            timeout_method: restrict::Method::Discord,
            timeout_role: None,
            voice_mute_method: restrict::Method::Discord,
            voice_mute_role: None,
        }
    }
}

/// A rule of `escalation`: when a warning brings a member to exactly `warns` active warnings,
/// `action` is taken. The fields are optional, so a rule without one is skipped rather than
/// refusing the whole configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    warns: Option<u32>,
    action: Option<Action>,
    /// How long: needed by a timeout, optional for a ban, not used by a kick.
    duration: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Timeout,
    Kick,
    Ban,
}

/// What a valid rule does, with its duration read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escalation {
    Timeout(Duration),
    Kick,
    Ban(Option<Duration>),
}

/// The escalation of a rule, or `None` when the rule is not valid: a timeout needs a readable
/// duration of at most 28 days, and a ban's duration, if given, must be readable.
fn plan(rule: &Rule) -> Option<Escalation> {
    match rule.action? {
        Action::Timeout => {
            let duration = parse_duration(rule.duration.as_deref()?).ok()?;
            (duration.as_secs() <= u64::from(MAX_TIMEOUT_SECONDS))
                .then_some(Escalation::Timeout(duration))
        }
        Action::Kick => Some(Escalation::Kick),
        Action::Ban => match rule.duration.as_deref() {
            Some(text) => Some(Escalation::Ban(Some(parse_duration(text).ok()?))),
            None => Some(Escalation::Ban(None)),
        },
    }
}

/// The rule that applies at `active` warnings: the first one that names that number.
fn rule_for(rules: &[Rule], active: usize) -> Option<&Rule> {
    let active = u32::try_from(active).ok()?;
    rules.iter().find(|rule| rule.warns == Some(active))
}

#[derive(CommandArgs)]
struct Warn {
    /// Member to warn
    user: UserId,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct Note {
    /// Member the note is about
    user: UserId,
    /// What to note
    #[arg(max_length = 512)]
    text: String,
}

#[derive(CommandArgs)]
struct Target {
    /// Member
    user: UserId,
}

#[derive(CommandArgs)]
struct Number {
    /// Number of the case
    number: i64,
}

#[derive(CommandArgs)]
struct Listing {
    /// Member
    user: UserId,
    /// Show only the cases numbered below this
    before: Option<i64>,
}

#[derive(CommandArgs)]
struct Revoke {
    /// Number of the case
    number: i64,
    /// Why it is revoked
    #[arg(max_length = 512)]
    reason: Option<String>,
}

/// A member and the reason of an action on it.
#[derive(CommandArgs)]
pub(crate) struct MemberReason {
    /// Member
    user: UserId,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct TimeoutArgs {
    /// Member to time out
    user: UserId,
    /// How long, such as 1h or 7d; at most 28 days
    duration: Duration,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct BanArgs {
    /// Member to ban
    user: UserId,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
    /// How long the ban lasts, such as 7d; forever when left out
    duration: Option<Duration>,
    /// How many days of the member's messages to delete, 0 to 7
    #[arg(min = 0, max = 7)]
    delete_days: Option<i64>,
}

/// The payload of a `unban` job: the member, and the case of the ban it lifts.
#[derive(Debug, Serialize, Deserialize)]
struct Unban {
    user: u64,
    case: i64,
}

/// Where a case stands at a given time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Active,
    Revoked,
    Expired,
}

impl State {
    fn key(self) -> &'static str {
        match self {
            Self::Active => "state-active",
            Self::Revoked => "state-revoked",
            Self::Expired => "state-expired",
        }
    }
}

/// A case is revoked once it is revoked, expired when its expiry is due, else active.
fn state(case: &Case, now: i64) -> State {
    if case.revoked.is_some() {
        State::Revoked
    } else if case.expires_at.is_some_and(|expires| expires <= now) {
        State::Expired
    } else {
        State::Active
    }
}

/// The one place that decides whether a case of `kind` still stands.
fn stands(case: &Case, kind: &str, now: i64) -> bool {
    case.kind.as_str() == kind && state(case, now) == State::Active
}

/// The cases of `kind` against `target` that still stand, newest first, at most `wanted`. Looks
/// back over at most [`SCAN`] cases.
async fn active_cases(
    cases: &Cases,
    target: UserId,
    kind: &str,
    now: i64,
    wanted: usize,
) -> Result<Vec<Case>, CaseError> {
    let mut found = Vec::new();
    let mut before = None;
    let mut looked = 0;
    while found.len() < wanted && looked < SCAN {
        let page = cases.for_target(target, before, PAGE).await?;
        looked += page.len();
        before = page.last().map(|case| case.number);
        found.extend(page.iter().filter(|case| stands(case, kind, now)).cloned());
        if page.len() < PAGE {
            break;
        }
    }
    found.truncate(wanted);
    Ok(found)
}

fn mention(user: UserId) -> String {
    format!("<@{}>", user.get())
}

fn count(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

/// A reason as the user typed it; a blank one is no reason.
fn reason_of(text: Option<String>) -> Option<String> {
    text.filter(|text| !text.trim().is_empty())
}

/// A reason as a list shows it: escaped and shortened.
fn shorten(reason: &str) -> String {
    truncate(&escape(reason), REASON_SHOWN)
}

/// A reason in a list, with the words for no reason when there is none.
fn listed_reason(cx: &CommandCx, reason: Option<&str>) -> String {
    reason.map_or_else(|| cx.t("reason-none", &Args::new()), shorten)
}

fn not_found(number: i64) -> UserError {
    UserError::message("case-not-found").with("number", number)
}

/// A length of time in seconds, as the clock and the cases count it.
fn secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

/// The reason of a punishment: a blank one is no reason, and the server may ask for one.
fn punishment_reason(config: &Config, text: Option<String>) -> Result<Option<String>, UserError> {
    let reason = reason_of(text);
    if config.require_reason && reason.is_none() {
        return Err(UserError::message("reason-required"));
    }
    Ok(reason)
}

/// The reason as a direct message says it, in the language of the server.
fn dm_reason(cx: &CommandCx, reason: Option<&str>) -> String {
    match reason {
        Some(text) => escape(text),
        None => cx.ctx().t_in(cx.ctx().locale, "reason-none", &Args::new()),
    }
}

/// The reason Discord's audit log keeps: who did it, then why.
fn audit_reason(cx: &CommandCx, reason: Option<&str>) -> Reason {
    let who = &cx.user().name;
    Reason::truncated(&match reason {
        Some(reason) => format!("{who}: {reason}"),
        None => who.to_owned(),
    })
}

/// Messages the member about an action, when the server wants that, in the server language. Returns
/// the note for the reply when the message did not arrive. A member who is not in the server is
/// not messaged and gets no note.
async fn notify(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    key: &str,
    args: &Args,
) -> Option<String> {
    if !config.dm_on_action {
        return None;
    }
    let text = cx.ctx().t_in(cx.ctx().locale, key, args);
    match cx.direct_message(user, DirectMessage::text(text)).await {
        Ok(Sent::Delivered(_))
        | Err(UserError::Module(ModuleError::Discord(DiscordError::NotFound))) => None,
        Ok(Sent::Closed) | Err(_) => Some(cx.t("dm-not-delivered", &Args::new())),
    }
}

/// A reply with its notes, each after a space.
fn with_notes(mut text: String, notes: &[String]) -> String {
    for note in notes {
        text.push(' ');
        text.push_str(note);
    }
    text
}

/// Answers the moderator in private, with the notes that apply.
async fn answer(cx: &CommandCx, key: &str, args: &Args, notes: &[String]) -> Result<(), UserError> {
    cx.reply_private(with_notes(cx.t(key, args), notes)).await
}

/// Records an action as a case and posts it to the log. A log that cannot take it does not undo
/// the action.
async fn record(
    cx: &CommandCx,
    kind: &str,
    target: UserId,
    reason: Option<String>,
    expires_at: Option<i64>,
) -> Result<Case, UserError> {
    let case = cx
        .cases()
        .create(NewCase {
            kind: CaseKind::new(kind)?,
            target,
            moderator: cx.user().id,
            reason,
            expires_at,
        })
        .await?;
    let _ = cx.modlog(&case).await;
    Ok(case)
}

/// Revokes the newest case of `kind` that still stands against `target`. Best effort: the Discord
/// action is done already, so a failure here does not fail the command.
async fn revoke_latest(cx: &CommandCx, target: UserId, kind: &str, reason: String) {
    if let Ok(cases) = active_cases(cx.cases(), target, kind, cx.now(), 1).await
        && let Some(case) = cases.first()
    {
        let _ = cx
            .cases()
            .revoke(case.number, cx.user().id, Some(reason))
            .await;
    }
}

/// The key of the timer that lifts the ban of `user`.
fn unban_key(user: UserId) -> String {
    format!("unban:{}", user.get())
}

/// Sets the timer that lifts the ban of `user` (case `case`) after `after`.
async fn schedule_unban(
    cx: &CommandCx,
    user: UserId,
    case: i64,
    after: Duration,
) -> Result<(), ScheduleError> {
    cx.scheduler()
        .job(
            "unban",
            &Unban {
                user: user.get(),
                case,
            },
        )?
        .key(&unban_key(user))
        .after(after)
        .await
}

/// The seconds of a timeout, which Discord takes at most 28 days of.
fn timeout_seconds(duration: Duration) -> Result<u32, UserError> {
    u32::try_from(duration.as_secs())
        .ok()
        .filter(|seconds| *seconds <= MAX_TIMEOUT_SECONDS)
        .ok_or_else(|| UserError::message("timeout-too-long"))
}

/// The seconds of messages a ban deletes, which Discord takes at most 7 days of.
fn delete_seconds(days: i64) -> Result<u32, UserError> {
    match u32::try_from(days * DAY) {
        Ok(seconds) if seconds <= MAX_BAN_DELETE_SECONDS => Ok(seconds),
        _ => Err(UserError::message("ban-delete-days-range")),
    }
}

/// Times a member out and records it, then messages the member: the member stays in the server,
/// so the message comes after. Returns the case and the note of the message.
async fn time_out(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    duration: Duration,
    reason: Option<String>,
) -> Result<(Case, Vec<String>), UserError> {
    let now = cx.now();
    let reason_text = dm_reason(cx, reason.as_deref());
    let role = restrict::give_timeout(cx, config, user, duration, reason.as_deref()).await?;
    let case = record(cx, "timeout", user, reason, Some(now + secs(duration))).await?;
    // The role of a timeout is taken off by a timer; a note says when no timer could be set.
    let timer_note = match role {
        Some(role) => restrict::schedule_timeout_lift(cx, user, case.number, role, duration)
            .await
            .is_err()
            .then(|| cx.t("timeout-no-timer", &Args::new())),
        None => None,
    };
    let note = notify(
        cx,
        config,
        user,
        "dm-timed-out",
        &Args::new()
            .with("duration", cx.format_duration_in(cx.ctx().locale, duration))
            .with("reason", reason_text),
    )
    .await;
    Ok((case, [note, timer_note].into_iter().flatten().collect()))
}

/// Kicks a member and records it. The direct message comes first: a kicked member shares no server
/// with the bot. Returns the case and the note of the message.
async fn kick_member(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    reason: Option<String>,
) -> Result<(Case, Vec<String>), UserError> {
    let note = notify(
        cx,
        config,
        user,
        "dm-kicked",
        &Args::new().with("reason", dm_reason(cx, reason.as_deref())),
    )
    .await;
    cx.api()
        .kick(cx.guild_id(), user, audit_reason(cx, reason.as_deref()))
        .await?;
    let case = record(cx, "kick", user, reason, None).await?;
    Ok((case, note.into_iter().collect()))
}

/// Bans a member for good or for `duration`, deleting `delete_message_seconds` of their messages.
/// The direct message comes first, as with a kick. A timed ban gets a timer that lifts it. Returns
/// the case and the notes: the message, and a note when the timer could not be set.
async fn ban_member(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    duration: Option<Duration>,
    delete_message_seconds: u32,
    reason: Option<String>,
) -> Result<(Case, Vec<String>), UserError> {
    let note = match duration {
        Some(duration) => {
            notify(
                cx,
                config,
                user,
                "dm-banned-for",
                &Args::new()
                    .with("duration", cx.format_duration_in(cx.ctx().locale, duration))
                    .with("reason", dm_reason(cx, reason.as_deref())),
            )
            .await
        }
        None => {
            notify(
                cx,
                config,
                user,
                "dm-banned",
                &Args::new().with("reason", dm_reason(cx, reason.as_deref())),
            )
            .await
        }
    };
    cx.api()
        .ban(
            cx.guild_id(),
            user,
            delete_message_seconds,
            audit_reason(cx, reason.as_deref()),
        )
        .await?;
    let expires_at = duration.map(|duration| cx.now() + secs(duration));
    let case = record(cx, "ban", user, reason, expires_at).await?;
    // A new ban replaces the timer of an earlier one, which would lift this ban too early.
    let _ = cx.scheduler().cancel(&unban_key(user)).await;
    // The ban holds even when its timer cannot be set; the note says so.
    let timer_note = match duration {
        Some(duration) => schedule_unban(cx, user, case.number, duration)
            .await
            .is_err()
            .then(|| cx.t("ban-no-timer", &Args::new())),
        None => None,
    };
    Ok((case, [note, timer_note].into_iter().flatten().collect()))
}

/// Carries out the escalation that the warning `warning` brought about, at `active` active
/// warnings. The reason is kept in the case, so it is written in the server language.
async fn escalate(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    escalation: Escalation,
    active: usize,
    warning: i64,
) -> Result<(Case, Vec<String>), UserError> {
    let reason = cx.ctx().t_in(
        cx.ctx().locale,
        "escalation-reason",
        &Args::new()
            .with("count", count(active))
            .with("case", warning),
    );
    match escalation {
        Escalation::Timeout(duration) => time_out(cx, config, user, duration, Some(reason)).await,
        Escalation::Kick => kick_member(cx, config, user, Some(reason)).await,
        Escalation::Ban(duration) => {
            let delete = delete_seconds(i64::from(config.ban_delete_days))?;
            ban_member(cx, config, user, duration, delete, Some(reason)).await
        }
    }
}

/// What an escalation does, in words, in the language of the moderator.
fn escalation_phrase(cx: &CommandCx, escalation: Escalation) -> String {
    match escalation {
        Escalation::Timeout(duration) => cx.t(
            "escalation-timeout",
            &Args::new().with("duration", cx.format_duration(duration)),
        ),
        Escalation::Kick => cx.t("escalation-kick", &Args::new()),
        Escalation::Ban(None) => cx.t("escalation-ban", &Args::new()),
        Escalation::Ban(Some(duration)) => cx.t(
            "escalation-ban-for",
            &Args::new().with("duration", cx.format_duration(duration)),
        ),
    }
}

/// The line of the reply about the escalation that the warning `warning` brings about, if a rule
/// asks for one. A failed escalation leaves the warning standing and says so.
async fn escalation_line(
    cx: &CommandCx,
    config: &Config,
    user: UserId,
    active: usize,
    warning: i64,
) -> Option<String> {
    let rule = rule_for(&config.escalation, active)?;
    let Some(escalation) = plan(rule) else {
        return Some(cx.t(
            "escalation-invalid",
            &Args::new().with("count", count(active)),
        ));
    };
    let phrase = escalation_phrase(cx, escalation);
    Some(
        match escalate(cx, config, user, escalation, active, warning).await {
            Ok((case, notes)) => with_notes(
                cx.t(
                    "escalated",
                    &Args::new()
                        .with("action", phrase)
                        .with("number", case.number),
                ),
                &notes,
            ),
            Err(_) => cx.t("escalation-failed", &Args::new().with("action", phrase)),
        },
    )
}

/// Warns a member. The warning is a case that expires after `warn_expire_days`. Once it brings the
/// member to a number of active warnings that a rule names, that rule's action follows.
async fn warn(cx: CommandCx, args: Warn) -> Result<(), UserError> {
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let reason = punishment_reason(&config, args.reason)?;
    let now = cx.now();
    let expires_at = now + i64::from(config.warn_expire_days) * DAY;
    let case = record(&cx, "warn", args.user, reason.clone(), Some(expires_at)).await?;
    let active = active_cases(cx.cases(), args.user, "warn", now, usize::MAX)
        .await?
        .len();

    // The warning is stored already: a member who cannot be messaged keeps it, and the reply says
    // that the message did not arrive instead of failing.
    let note = notify(
        &cx,
        &config,
        args.user,
        "dm-warned",
        &Args::new()
            .with("reason", dm_reason(&cx, reason.as_deref()))
            .with("active", count(active)),
    )
    .await;
    let escalation = escalation_line(&cx, &config, args.user, active, case.number).await;
    let notes: Vec<String> = note.into_iter().collect();
    let mut text = with_notes(
        cx.t(
            "warned",
            &Args::new()
                .with("user", mention(args.user))
                .with("number", case.number)
                .with("active", count(active)),
        ),
        &notes,
    );
    text.extend(escalation.map(|line| format!("\n{line}")));
    cx.reply_private(text).await
}

/// Notes something about a member. A note is a case without expiry and is not a warning.
async fn note(cx: CommandCx, args: Note) -> Result<(), UserError> {
    let case = record(&cx, "note", args.user, Some(args.text), None).await?;
    cx.reply_private_t(
        "noted",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
    )
    .await
}

/// Lists the warnings of a member that still count, newest first.
async fn warnings(cx: CommandCx, args: Target) -> Result<(), UserError> {
    let now = cx.now();
    let found = active_cases(cx.cases(), args.user, "warn", now, SHOWN).await?;
    if found.is_empty() {
        return cx
            .reply_private_t(
                "warnings-none",
                &Args::new().with("user", mention(args.user)),
            )
            .await;
    }
    let mut text = cx.t(
        "warnings-header",
        &Args::new()
            .with("user", mention(args.user))
            .with("count", count(found.len())),
    );
    for case in &found {
        let expires = case.expires_at.map_or_else(
            || cx.t("expires-never", &Args::new()),
            |at| timestamp(at, TimeStyle::Relative),
        );
        let line = cx.t(
            "warnings-line",
            &Args::new()
                .with("number", case.number)
                .with("reason", listed_reason(&cx, case.reason.as_deref()))
                .with("expires", expires),
        );
        if text.chars().count() + 1 + line.chars().count() > MAX_CHARS {
            break;
        }
        text.push('\n');
        text.push_str(&line);
    }
    cx.reply_private(text).await
}

/// Shows a case the way the log does.
async fn case_view(cx: CommandCx, args: Number) -> Result<(), UserError> {
    let Some(case) = cx.cases().get(args.number).await? else {
        return Err(not_found(args.number));
    };
    let embed = modlog::embed(cx.ctx(), &case).build()?;
    cx.reply_with(InteractionReply {
        embeds: vec![embed],
        ephemeral: true,
        ..InteractionReply::default()
    })
    .await
}

/// Lists the cases of a member, newest first, `before` a number if given.
async fn case_list(cx: CommandCx, args: Listing) -> Result<(), UserError> {
    let cases = cx
        .cases()
        .for_target(args.user, args.before, LISTED)
        .await?;
    if cases.is_empty() {
        return cx
            .reply_private_t(
                "case-list-none",
                &Args::new().with("user", mention(args.user)),
            )
            .await;
    }
    let now = cx.now();
    let mut text = cx.t(
        "case-list-header",
        &Args::new().with("user", mention(args.user)),
    );
    for case in &cases {
        let line = cx.t(
            "case-list-line",
            &Args::new()
                .with("number", case.number)
                .with("kind", case.kind.as_str().to_owned())
                .with("state", cx.t(state(case, now).key(), &Args::new()))
                .with("reason", listed_reason(&cx, case.reason.as_deref())),
        );
        text.push('\n');
        text.push_str(&line);
    }
    cx.reply_private(text).await
}

/// Revokes a case, once; the log shows it grey.
async fn case_revoke(cx: CommandCx, args: Revoke) -> Result<(), UserError> {
    let revoked = match cx
        .cases()
        .revoke(args.number, cx.user().id, reason_of(args.reason))
        .await
    {
        Ok(case) => case,
        Err(CaseError::NotFound) => return Err(not_found(args.number)),
        Err(CaseError::AlreadyRevoked) => {
            return Err(UserError::message("case-already-revoked").with("number", args.number));
        }
        Err(error) => return Err(error.into()),
    };
    let _ = cx.modlog(&revoked).await;
    cx.reply_private_t("case-revoked", &Args::new().with("number", args.number))
        .await
}

/// Times a member out. Discord's own timeout is 28 days at most, so a longer one is refused before
/// anything is done; a timeout by role has no such limit. The member stays in the server, so the
/// direct message comes after the timeout.
async fn timeout(cx: CommandCx, args: TimeoutArgs) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    restrict::check_timeout(&config, args.duration)?;
    cx.check_target(args.user).await?;
    let reason = punishment_reason(&config, args.reason)?;
    let (case, notes) = time_out(&cx, &config, args.user, args.duration, reason).await?;
    let duration = cx.format_duration(args.duration);
    answer(
        &cx,
        "timed-out",
        &Args::new()
            .with("user", mention(args.user))
            .with("duration", duration)
            .with("number", case.number),
        &notes,
    )
    .await
}

/// Lifts a timeout. The newest timeout that still stands is revoked with it.
async fn untimeout(cx: CommandCx, args: MemberReason) -> Result<(), UserError> {
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let reason = reason_of(args.reason);
    restrict::lift_timeout(&cx, &config, args.user, reason.as_deref()).await?;
    let case = record(&cx, "untimeout", args.user, reason, None).await?;
    revoke_latest(
        &cx,
        args.user,
        "timeout",
        format!("removed by case #{}", case.number),
    )
    .await;
    cx.reply_private_t(
        "untimed-out",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
    )
    .await
}

/// Kicks a member. The direct message comes first: a kicked member shares no server with the bot.
async fn kick(cx: CommandCx, args: MemberReason) -> Result<(), UserError> {
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let reason = punishment_reason(&config, args.reason)?;
    let (case, notes) = kick_member(&cx, &config, args.user, reason).await?;
    answer(
        &cx,
        "kicked",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
        &notes,
    )
    .await
}

/// Bans a member, for good or for a while. A timed ban gets a timer that lifts it when its time is
/// up. The direct message comes first, as with a kick.
async fn ban(cx: CommandCx, args: BanArgs) -> Result<(), UserError> {
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let reason = punishment_reason(&config, args.reason)?;
    let days = args
        .delete_days
        .unwrap_or(i64::from(config.ban_delete_days));
    let (case, notes) = ban_member(
        &cx,
        &config,
        args.user,
        args.duration,
        delete_seconds(days)?,
        reason,
    )
    .await?;

    let mut words = Args::new()
        .with("user", mention(args.user))
        .with("number", case.number);
    let key = match args.duration {
        Some(duration) => {
            words = words.with("duration", cx.format_duration(duration));
            "banned-for"
        }
        None => "banned",
    };
    answer(&cx, key, &words, &notes).await
}

/// Lifts a ban. The timer of a timed ban goes, and the newest ban that still stands is revoked.
async fn unban(cx: CommandCx, args: MemberReason) -> Result<(), UserError> {
    cx.check_target(args.user).await?;
    let reason = reason_of(args.reason);
    cx.api()
        .unban(
            cx.guild_id(),
            args.user,
            audit_reason(&cx, reason.as_deref()),
        )
        .await?;
    let case = record(&cx, "unban", args.user, reason, None).await?;
    // Both are best effort: the member is unbanned already.
    let _ = cx.scheduler().cancel(&unban_key(args.user)).await;
    revoke_latest(
        &cx,
        args.user,
        "ban",
        format!("unbanned by case #{}", case.number),
    )
    .await;
    cx.reply_private_t(
        "unbanned",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
    )
    .await
}

/// Lifts a timed ban when its time is up. A member who was unbanned by hand is done already, so
/// the job may run twice.
async fn lift_ban(cx: JobCx) -> Result<(), ModuleError> {
    let payload: Unban = cx.payload()?;
    let Some(user) = UserId::new_checked(payload.user) else {
        return Err(ModuleError::other("a ban job names no member"));
    };
    let reason = Reason::truncated(&format!("the ban of case #{} ran out", payload.case));
    match cx.api().unban(cx.guild_id(), user, reason).await {
        Ok(()) | Err(DiscordError::NotFound) => Ok(()),
        Err(error) => Err(ModuleError::Discord(error)),
    }
}

/// The module, still open for a test to build on.
/// The settings a server can change: their schema, and their defaults.
fn settings() -> (serde_json::Value, serde_json::Value) {
    (
        json!({
            "type": "object",
            "properties": {
                "warn_expire_days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 365,
                    "description": "After how many days a warning expires."
                },
                "dm_on_action": {
                    "type": "boolean",
                    "description": "Whether the warned member gets a direct message."
                },
                "require_reason": {
                    "type": "boolean",
                    "description": "Whether a warning needs a reason."
                },
                "ban_delete_days": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 7,
                    "description": "How many days of the messages of a banned member are deleted."
                },
                "escalation": {
                    "type": "array",
                    "maxItems": 10,
                    "description": "What a warning brings about when a member reaches a number of active warnings.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "warns": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 100,
                                "description": "The number of active warnings that triggers the action."
                            },
                            "action": {
                                "type": "string",
                                "enum": ["timeout", "kick", "ban"]
                            },
                            "duration": {
                                "type": "string",
                                "pattern": "^([0-9]{1,10}[smhdw] ?)+$",
                                "description": "How long, such as 1h or 7d. A timeout needs it; a ban may have it."
                            }
                        },
                        "required": ["warns", "action"],
                        "additionalProperties": false
                    }
                },
                "timeout_method": {
                    "type": "string",
                    "enum": ["discord", "role"],
                    "description": "How a timeout is given: Discord's timeout, or a role that is taken off when its time is up."
                },
                "timeout_role": {
                    "type": ["string", "null"],
                    "pattern": "^[0-9]{1,20}$",
                    "description": "The id of the role a timeout gives, when timeout_method is role."
                },
                "voice_mute_method": {
                    "type": "string",
                    "enum": ["discord", "role"],
                    "description": "How a voice mute is given: Discord's voice mute, or a role that is taken off when its time is up."
                },
                "voice_mute_role": {
                    "type": ["string", "null"],
                    "pattern": "^[0-9]{1,20}$",
                    "description": "The id of the role a voice mute gives, when voice_mute_method is role."
                }
            },
            "additionalProperties": false
        }),
        json!({
            "warn_expire_days": 30,
            "dm_on_action": true,
            "require_reason": false,
            "ban_delete_days": 0,
            "escalation": [],
            "timeout_method": "discord",
            "timeout_role": null,
            "voice_mute_method": "discord",
            "voice_mute_role": null
        }),
    )
}

/// The commands that act on members, and the job that lifts a timed ban.
fn punishments(builder: ModuleBuilder) -> ModuleBuilder {
    builder
        .command(
            Command::<TimeoutArgs>::new("timeout", "Time a member out")
                .requires("moderation.timeout")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(timeout),
        )
        .command(
            Command::<MemberReason>::new("untimeout", "Lift the timeout of a member")
                .requires("moderation.timeout")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(untimeout),
        )
        .command(
            Command::<MemberReason>::new("kick", "Kick a member")
                .requires("moderation.kick")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(kick),
        )
        .command(
            Command::<BanArgs>::new("ban", "Ban a member, for good or for a while")
                .requires("moderation.ban")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(ban),
        )
        .command(
            Command::<MemberReason>::new("unban", "Lift the ban of a member")
                .requires("moderation.ban")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(unban),
        )
        .command(
            Command::<restrict::VoiceMute>::new("vcmute", "Mute a member in voice channels")
                .requires("moderation.voice")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(restrict::voice_mute),
        )
        .command(
            Command::<MemberReason>::new("vcunmute", "Lift the voice mute of a member")
                .requires("moderation.voice")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(restrict::voice_unmute),
        )
        .job("unban", lift_ban)
        .job("untimeout", restrict::end_timeout)
        .job("vc_unmute", restrict::end_voice_mute)
}

/// The module, still open for a test to build on.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = settings();
    let builder = ModuleBuilder::new("moderation", "Moderation", env!("CARGO_PKG_VERSION"))?
        .description("Warn members, note about them, and look at and revoke the cases of a server.")
        .capability(Capability::Cases)
        .capability(Capability::SendMessages)
        .capability(Capability::ReadGuild)
        .capability(Capability::SendDirectMessages)
        .capability(Capability::KickMembers)
        .capability(Capability::BanMembers)
        .capability(Capability::TimeoutMembers)
        .capability(Capability::ManageRoles)
        .capability(Capability::MuteMembers)
        .permission(PermissionSpec::new(
            "moderation.warn",
            Level::MODERATOR,
            "Warn members",
        )?)
        .permission(PermissionSpec::new(
            "moderation.note",
            Level::MODERATOR,
            "Add notes about members",
        )?)
        .permission(PermissionSpec::new(
            "moderation.cases",
            Level::MODERATOR,
            "See the cases and warnings of members",
        )?)
        .permission(PermissionSpec::new(
            "moderation.revoke",
            Level::MODERATOR,
            "Revoke cases",
        )?)
        .permission(PermissionSpec::new(
            "moderation.timeout",
            Level::MODERATOR,
            "Time members out and lift timeouts",
        )?)
        .permission(PermissionSpec::new(
            "moderation.kick",
            Level::MODERATOR,
            "Kick members",
        )?)
        .permission(PermissionSpec::new(
            "moderation.ban",
            Level::ADMIN,
            "Ban and unban members",
        )?)
        .permission(PermissionSpec::new(
            "moderation.voice",
            Level::MODERATOR,
            "Mute members in voice channels and lift it",
        )?)
        .config(schema, defaults)
        .command(
            Command::<Warn>::new("warn", "Warn a member")
                .requires("moderation.warn")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(warn),
        )
        .command(
            Command::<Note>::new("note", "Add a note about a member")
                .requires("moderation.note")
                .handler(note),
        )
        .command(
            Command::<Target>::new("warnings", "Show the active warnings of a member")
                .requires("moderation.cases")
                .handler(warnings),
        )
        .group(
            "case",
            "Look at, list and revoke cases",
            [
                Command::<Number>::new("view", "Show a case")
                    .requires("moderation.cases")
                    .handler(case_view),
                Command::<Listing>::new("list", "List the cases of a member")
                    .requires("moderation.cases")
                    .handler(case_list),
                Command::<Revoke>::new("revoke", "Revoke a case")
                    .requires("moderation.revoke")
                    .handler(case_revoke),
            ],
        );
    Ok(punishments(builder)
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA))
}

/// The module as the host loads it.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}
