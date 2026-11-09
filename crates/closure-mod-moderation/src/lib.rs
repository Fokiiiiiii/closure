//! The `moderation` module: warnings, notes and the cases of a server.
//!
//! Written with the SDK only. It keeps no data of its own: every warning and note is a case of
//! the SDK's cases, numbered per server and shared by the modules. A warning counts until it is
//! revoked or its expiry is due, so nothing has to run later to expire one.

use std::sync::Arc;
use std::time::Duration;

use closure_sdk::duration::{TimeStyle, ends_at, parse as parse_duration, timestamp};
use closure_sdk::embed::{escape, truncate};
use closure_sdk::modlog;
use closure_sdk::punish::{
    self, Done, Method, Moderator, Punisher, RestrictionRole, RoleSetting, Settings, Strip,
    ensure_role,
};
use closure_sdk::{
    Args, Capability, Case, CaseError, CaseKind, Cases, Command, CommandArgs, CommandCx, Cooldown,
    CoreError, Cx, Event, EventContext, EventKind, Flow, InteractionRef, InteractionReply, Level,
    Locale, ModuleBuilder, ModuleError, NewCase, PermissionSpec, RoleId, SdkModule, Span,
    Subscription, Tone, UserError, UserId,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::gateway::Intents;

mod appeals;
mod bulk;
mod casetools;
mod channel_controls;
mod clear;
mod history;
mod manual;
mod menu;
mod rejoin;
mod restrict;
mod softban;
mod templates;
mod undo;
mod userinfo;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const DAY: i64 = 86_400;
/// Discord's longest timeout, 28 days in seconds.
const MAX_TIMEOUT_SECONDS: u32 = 28 * 86_400;
/// The most messages a ban deletes, 7 days in seconds. The argument's bounds are only a hint to
/// Discord, so the module checks them too.
const MAX_BAN_DELETE_SECONDS: u32 = 7 * 86_400;
const SHOWN: usize = 10;
const LISTED: usize = 10;
const PAGE: usize = 25;
const MAX_STANDING: usize = 10_000;
/// How many cases back an active warning is looked for. shortcut: a member with more cases
/// than this has the older warnings left out of the count, upgrade with an index on the target.
const SCAN: usize = 100;
const REASON_SHOWN: usize = 80;
/// Discord's limit of a message is 2000 characters; `/warnings` stays below it.
const MAX_CHARS: usize = 1900;

/// The module's section of a guild's configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Config {
    /// After how many days a warning expires.
    warn_expire_days: u32,
    /// Whether the warned member gets a direct message.
    dm_on_action: bool,
    dm_messages: punish::DmMessages,
    /// Whether a warning needs a reason.
    require_reason: bool,
    /// How many days of messages a ban deletes.
    ban_delete_days: u32,
    /// Whether a warning can bring about a punishment. Off keeps the rules, and acts on none.
    escalation_enabled: bool,
    /// What a warning brings about once a member reaches a number of active warnings.
    escalation: Vec<Rule>,
    /// How a timeout is given: Discord's timeout, or a role that a timer takes off again.
    timeout_method: Method,
    /// The id of the role a timeout gives, when `timeout_method` is `role`.
    timeout_role: Option<String>,
    /// How a voice mute is given: Discord's voice mute, or a role that a timer takes off again.
    voice_mute_method: Method,
    /// The id of the role a voice mute gives, when `voice_mute_method` is `role`.
    voice_mute_role: Option<String>,
    /// Which roles a role timeout or voice mute takes off the member: `false`, `true` (every role
    /// that can be taken off), or role ids.
    remove_roles_on_mute: RoleSetting,
    /// Which of the roles taken off come back when the punishment ends: the same forms, `true`
    /// by default.
    restore_roles_on_mute: RoleSetting,
    /// Whether a ban, kick or timeout made by hand in Discord becomes a case.
    manual_cases: bool,
    /// The staff channel the case exports (CSV files) are sent to.
    export_channel: Option<String>,
    /// The channel where what needs a second moderator, and the appeals, are put for a decision.
    approval_channel: Option<String>,
    /// A bulk operation of this many members or more needs approval; 0 asks for none.
    approval_bulk_from: u32,
    /// A ban for good needs approval.
    approval_permanent_ban: bool,
    /// The roles a slowmode kept by the bot (longer than Discord's) does not hold back.
    slowmode_exempt_roles: Vec<String>,
    /// Whether the staff are told when a member with cases comes back.
    alert_on_rejoin: bool,
    /// The staff channel of that alert.
    alert_channel: Option<String>,
    /// Whether the templates are offered. Off keeps them, and offers none.
    templates_enabled: bool,
    /// The templates a moderator picks from when a timeout, kick, ban or voice mute lacks a reason
    /// or a duration.
    templates: templates::Templates,
}

impl Config {
    fn approval_target(&self) -> Option<closure_sdk::ChannelId> {
        self.approval_channel
            .as_deref()
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|id| *id > 0)
            .map(closure_sdk::ChannelId::new)
    }

    fn export_target(&self) -> Option<closure_sdk::ChannelId> {
        self.export_channel
            .as_deref()
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|id| *id > 0)
            .map(closure_sdk::ChannelId::new)
    }

    fn alert_target(&self) -> Option<closure_sdk::ChannelId> {
        self.alert_channel
            .as_deref()
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|id| *id > 0)
            .map(closure_sdk::ChannelId::new)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            warn_expire_days: 30,
            dm_on_action: true,
            dm_messages: punish::DmMessages::default(),
            require_reason: false,
            ban_delete_days: 0,
            escalation_enabled: true,
            escalation: vec![
                Rule {
                    warns: Some(3),
                    action: Some(Action::Timeout),
                    duration: Some("1d".to_owned()),
                },
                Rule {
                    warns: Some(5),
                    action: Some(Action::Kick),
                    duration: None,
                },
            ],
            timeout_method: Method::Discord,
            timeout_role: None,
            voice_mute_method: Method::Discord,
            voice_mute_role: None,
            remove_roles_on_mute: RoleSetting::Flag(false),
            restore_roles_on_mute: RoleSetting::Flag(true),
            manual_cases: true,
            export_channel: None,
            approval_channel: None,
            approval_bulk_from: 0,
            approval_permanent_ban: false,
            slowmode_exempt_roles: Vec::new(),
            alert_on_rejoin: false,
            alert_channel: None,
            templates_enabled: true,
            templates: templates::Templates::default(),
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

/// The first rule that names the number of active warnings.
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
struct ViewArgs {
    /// Number of the case, to see one
    number: Option<i64>,
    /// Member, to list their cases
    user: Option<UserId>,
    /// With a member: show only the cases numbered below this
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
    /// How long, such as 1h or 7d, or until a date and time; at most 28 days. A template may give it.
    duration: Option<Span>,
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
    /// How long the ban lasts, such as 7d, or until a date and time; forever when left out
    duration: Option<Span>,
    /// How many days of the member's messages to delete, 0 to 7
    #[arg(min = 0, max = 7)]
    delete_days: Option<i64>,
    /// Yes: a softban, the ban is lifted at once and only the messages are gone
    soft: Option<bool>,
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

/// Found in the store, so a punishment is found however many notes came after it.
async fn active_cases(
    cases: &Cases,
    target: UserId,
    kind: &str,
    now: i64,
    wanted: usize,
) -> Result<Vec<Case>, CaseError> {
    cases
        .standing(target, kind, now, wanted.min(MAX_STANDING))
        .await
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

fn shorten(reason: &str) -> String {
    truncate(&escape(reason), REASON_SHOWN)
}

/// A reason in a list, with the words for no reason when there is none.
fn listed_reason<I: InteractionRef + 'static>(cx: &Cx<I>, reason: Option<&str>) -> String {
    reason.map_or_else(|| cx.t("reason-none", &Args::new()), shorten)
}

fn not_found(number: i64) -> UserError {
    UserError::message("case-not-found").with("number", number)
}

/// The reason of a punishment: a blank one is no reason, and the server may ask for one.
fn punishment_reason(config: &Config, text: Option<String>) -> Result<Option<String>, UserError> {
    let reason = reason_of(text);
    if config.require_reason && reason.is_none() {
        return Err(UserError::message("reason-required"));
    }
    Ok(reason)
}

/// The punisher of a command or a click: the user who runs it, the server's settings, and notes in
/// the language of the reply. A role method gets its role here, created when it is missing.
async fn punisher_for<'a, I: InteractionRef + 'static>(
    cx: &'a Cx<I>,
    config: &Config,
) -> Result<Punisher<'a>, UserError> {
    let settings = punish_settings(cx.ctx(), config).await?;
    Ok(Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings).reply_locale(cx.locale()))
}

/// What the server chose for the punishments, as the SDK takes it. A role method's role is the
/// configured one, or the one Closure finds or creates (see [`ensure_role`]).
pub(crate) async fn punish_settings(
    ctx: &EventContext,
    config: &Config,
) -> Result<Settings, UserError> {
    let timeout_role = match config.timeout_method {
        Method::Role => Some(
            ensure_role(
                ctx,
                RestrictionRole::Timeout,
                restrict::role_of(config.timeout_role.as_deref()),
            )
            .await?,
        ),
        Method::Discord => None,
    };
    let voice_role = match config.voice_mute_method {
        Method::Role => Some(
            ensure_role(
                ctx,
                RestrictionRole::Voice,
                restrict::role_of(config.voice_mute_role.as_deref()),
            )
            .await?,
        ),
        Method::Discord => None,
    };
    Ok(settings_from(config, timeout_role, voice_role))
}

/// The SDK's settings of the configuration, with the roles the methods give. A punishment's timer
/// reads only what the restore setting says, so a rejoin may take these from the configuration too.
pub(crate) fn settings_from(
    config: &Config,
    timeout_role: Option<RoleId>,
    voice_role: Option<RoleId>,
) -> Settings {
    Settings {
        dm: config.dm_on_action,
        dm_messages: config.dm_messages.clone(),
        timeout: config.timeout_method,
        timeout_role,
        voice: config.voice_mute_method,
        voice_role,
        strip: Strip {
            remove: config.remove_roles_on_mute.selection(),
            restore: config.restore_roles_on_mute.selection(),
        },
    }
}

/// Sets the dedicated roles up when the configuration changes, so that they exist and are set up
/// in every channel before the first punishment.
async fn config_changed(ctx: EventContext, _event: Event) -> Result<Flow, ModuleError> {
    let config: Config = closure_sdk::parse_module_config(&ctx)?;
    if config.timeout_method == Method::Role {
        ensure_role(
            &ctx,
            RestrictionRole::Timeout,
            restrict::role_of(config.timeout_role.as_deref()),
        )
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    if config.voice_mute_method == Method::Role {
        ensure_role(
            &ctx,
            RestrictionRole::Voice,
            restrict::role_of(config.voice_mute_role.as_deref()),
        )
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    Ok(Flow::Continue)
}

/// A channel created later gets the overwrites of the dedicated roles, so it keeps the restrictions.
async fn channel_created(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    if let Event::ChannelCreated(channel) = event {
        punish::channel_created(&ctx, channel.id).await?;
    }
    Ok(Flow::Continue)
}

fn case_and_notes(punisher: &Punisher<'_>, done: Done) -> (Case, Vec<String>) {
    let notes = done
        .notes
        .iter()
        .map(|note| punisher.note_text(note))
        .collect();
    (done.case, notes)
}

fn with_notes(mut text: String, notes: &[String]) -> String {
    for note in notes {
        text.push(' ');
        text.push_str(note);
    }
    text
}

/// Records an action as a case and posts it to the log. A log that cannot take it does not undo
/// the action.
async fn record<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    kind: &str,
    target: UserId,
    reason: Option<String>,
    expires_at: Option<i64>,
) -> Result<Case, UserError> {
    let case = cx
        .cases()
        .create(NewCase {
            source: closure_sdk::CaseSource::Command,
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

fn timeout_seconds(duration: Duration) -> Result<u32, UserError> {
    u32::try_from(duration.as_secs())
        .ok()
        .filter(|seconds| *seconds <= MAX_TIMEOUT_SECONDS)
        .ok_or_else(|| UserError::message("timeout-too-long"))
}

fn delete_seconds(days: i64) -> Result<u32, UserError> {
    match u32::try_from(days * DAY) {
        Ok(seconds) if seconds <= MAX_BAN_DELETE_SECONDS => Ok(seconds),
        _ => Err(UserError::message("ban-delete-days-range")),
    }
}

/// Carries out the escalation that the warning `warning` brought about, at `active` active
/// warnings. The reason is kept in the case, so it is written in the server language.
async fn escalate<I: InteractionRef + 'static>(
    cx: &Cx<I>,
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
    let punisher = punisher_for(cx, config).await?;
    let done = match escalation {
        Escalation::Timeout(duration) => punisher.timeout(user, duration, Some(reason)).await?,
        Escalation::Kick => punisher.kick(user, Some(reason)).await?,
        Escalation::Ban(duration) => {
            let delete = delete_seconds(i64::from(config.ban_delete_days))?;
            punisher.ban(user, duration, delete, Some(reason)).await?
        }
    };
    Ok(case_and_notes(&punisher, done))
}

fn escalation_phrase<I: InteractionRef + 'static>(cx: &Cx<I>, escalation: Escalation) -> String {
    match escalation {
        Escalation::Timeout(duration) => cx.t(
            "escalation-timeout",
            &Args::new().with(
                "until",
                ends_at(cx.now(), duration, TimeStyle::ShortDateTime),
            ),
        ),
        Escalation::Kick => cx.t("escalation-kick", &Args::new()),
        Escalation::Ban(None) => cx.t("escalation-ban", &Args::new()),
        Escalation::Ban(Some(duration)) => cx.t(
            "escalation-ban-for",
            &Args::new().with(
                "until",
                ends_at(cx.now(), duration, TimeStyle::ShortDateTime),
            ),
        ),
    }
}

async fn escalation_line<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    config: &Config,
    user: UserId,
    active: usize,
    warning: i64,
) -> Option<String> {
    if !config.escalation_enabled {
        return None;
    }
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
async fn warn<I: InteractionRef + 'static>(cx: Cx<I>, args: Warn) -> Result<(), UserError> {
    // Acknowledged first: the work below may outlast Discord's three seconds.
    cx.defer(true).await?;
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let reason = punishment_reason(&config, args.reason)?;
    let now = cx.now();
    let expires_at = now + i64::from(config.warn_expire_days) * DAY;
    // The warning is stored already: a member who cannot be messaged keeps it, and the reply says
    // that the message did not arrive instead of failing.
    let punisher = punisher_for(&cx, &config).await?;
    // The warning and the count of the warnings are one step for the member: two warnings at once
    // each see the number they made, so a rule's number is reached exactly once. The turn is let go
    // before the rule acts, since a punishment takes the member's turn itself.
    let (done, active) = {
        let _turn = closure_sdk::punish::member_turn(cx.ctx(), args.user).await;
        let done = punisher.warn(args.user, reason, Some(expires_at)).await?;
        let active = active_cases(cx.cases(), args.user, "warn", now, usize::MAX)
            .await?
            .len();
        (done, active)
    };
    let (case, notes) = case_and_notes(&punisher, done);
    let escalation = escalation_line(&cx, &config, args.user, active, case.number).await;
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
    let embed = modlog::embed(cx.ctx(), &case).description(text).build()?;
    cx.reply_with(InteractionReply {
        embeds: vec![embed],
        components: undo::row(Some(undo::button(
            &cx,
            undo::Kind::Warn,
            args.user,
            case.number,
        ))),
        ephemeral: true,
        ..InteractionReply::default()
    })
    .await
}

/// Notes something about a member. A note is a case without expiry and is not a warning.
async fn note<I: InteractionRef + 'static>(cx: Cx<I>, args: Note) -> Result<(), UserError> {
    let case = record(&cx, "note", args.user, Some(args.text), None).await?;
    cx.say_private_t(
        Tone::Success,
        "noted",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
    )
    .await
}

/// The warnings of `user`; with `own` the ones moderators hid from the lists are left out, as
/// they are from the list of cases.
async fn show_warnings(cx: CommandCx, user: UserId, own: bool) -> Result<(), UserError> {
    let now = cx.now();
    let mut found = active_cases(cx.cases(), user, "warn", now, SHOWN).await?;
    if own {
        found.retain(|case| !case.hidden);
    }
    if found.is_empty() {
        return cx
            .reply_private_t("warnings-none", &Args::new().with("user", mention(user)))
            .await;
    }
    let mut text = cx.t(
        "warnings-header",
        &Args::new()
            .with("user", mention(user))
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

async fn warnings(cx: CommandCx, args: Target) -> Result<(), UserError> {
    show_warnings(cx, args.user, false).await
}

/// The same list for the member who asks, who needs no power for it.
async fn mywarnings(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let user = cx.user().id;
    show_warnings(cx, user, true).await
}

/// Shows a case the way the log does, or lists the cases of a member.
async fn case_view(cx: CommandCx, args: ViewArgs) -> Result<(), UserError> {
    match (args.number, args.user) {
        (Some(number), _) => show_case(cx, number).await,
        (None, Some(user)) => case_list(cx, user, args.before).await,
        (None, None) => Err(UserError::message("case-view-needs-one")),
    }
}

async fn show_case(cx: CommandCx, number: i64) -> Result<(), UserError> {
    let Some(case) = cx.cases().get(number).await? else {
        return Err(not_found(number));
    };
    let embed = modlog::embed(cx.ctx(), &case).build()?;
    cx.reply_with(InteractionReply {
        embeds: vec![embed],
        ephemeral: true,
        ..InteractionReply::default()
    })
    .await
}

async fn case_list<I: InteractionRef + 'static>(
    cx: Cx<I>,
    user: UserId,
    before: Option<i64>,
) -> Result<(), UserError> {
    let mut cases = cx.cases().for_target(user, before, LISTED).await?;
    // A hidden case is shown by its number only.
    cases.retain(|case| !case.hidden);
    if cases.is_empty() {
        return cx
            .reply_private_t("case-list-none", &Args::new().with("user", mention(user)))
            .await;
    }
    let now = cx.now();
    let mut text = cx.t("case-list-header", &Args::new().with("user", mention(user)));
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
    cx.say_private_t(
        Tone::Success,
        "case-revoked",
        &Args::new().with("number", args.number),
    )
    .await
}

/// Times a member out; a timeout longer than Discord's 28 days is refused (a role timeout has no limit).
async fn timeout<I: InteractionRef + 'static>(
    cx: Cx<I>,
    args: TimeoutArgs,
) -> Result<(), UserError> {
    cx.defer(true).await?;
    // Resolved first: a moment that has passed is refused before anything else is asked.
    let duration = args
        .duration
        .as_ref()
        .map(|span| span.resolve(&cx))
        .transpose()?;
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    restrict::check_timeout(&config, duration)?;
    let punishment = templates::Punishment::new(
        templates::Kind::Timeout,
        args.user,
        reason_of(args.reason),
        args.duration,
        None,
    );
    templates::punish(&cx, &config, punishment).await
}

/// Lifts a timeout. The newest timeout that still stands is revoked with it.
async fn untimeout(cx: CommandCx, args: MemberReason) -> Result<(), UserError> {
    cx.defer(true).await?;
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let case = punisher_for(&cx, &config)
        .await?
        .lift_timeout(args.user, reason_of(args.reason))
        .await?;
    cx.say_private_t(
        Tone::Success,
        "untimed-out",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
    )
    .await
}

/// Kicks a member. The direct message comes first: a kicked member shares no server with the bot.
async fn kick<I: InteractionRef + 'static>(cx: Cx<I>, args: MemberReason) -> Result<(), UserError> {
    cx.defer(true).await?;
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let punishment = templates::Punishment::new(
        templates::Kind::Kick,
        args.user,
        reason_of(args.reason),
        None,
        None,
    );
    templates::punish(&cx, &config, punishment).await
}

async fn ban<I: InteractionRef + 'static>(cx: Cx<I>, args: BanArgs) -> Result<(), UserError> {
    if args.soft == Some(true) {
        if args.duration.is_some() {
            return Err(UserError::message("ban-soft-no-duration"));
        }
        return softban::softban(cx, args.user, args.reason, args.delete_days).await;
    }
    cx.defer(true).await?;
    if let Some(span) = &args.duration {
        span.resolve(&cx)?;
    }
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    // Refused here, before a template is asked for: the ban would refuse the same days.
    delete_seconds(
        args.delete_days
            .unwrap_or(i64::from(config.ban_delete_days)),
    )?;
    // A ban for good may need a second moderator, unless the one who asks may approve it.
    if config.approval_permanent_ban
        && args.duration.is_none()
        && cx.require("moderation.approve").await.is_err()
    {
        let Some(reason) = reason_of(args.reason.clone()) else {
            return Err(UserError::message("bulk-approval-needs-reason"));
        };
        return bulk::request_ban(
            &cx,
            &config,
            args.user,
            reason,
            args.delete_days
                .unwrap_or(i64::from(config.ban_delete_days)),
        )
        .await;
    }
    let punishment = templates::Punishment::new(
        templates::Kind::Ban,
        args.user,
        reason_of(args.reason),
        args.duration,
        args.delete_days,
    );
    templates::punish(&cx, &config, punishment).await
}

/// Lifts a ban. The timer of a timed ban goes, and the newest ban that still stands is revoked.
async fn unban(cx: CommandCx, args: MemberReason) -> Result<(), UserError> {
    cx.defer(true).await?;
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let case = punisher_for(&cx, &config)
        .await?
        .unban(args.user, reason_of(args.reason))
        .await?;
    cx.say_private_t(
        Tone::Success,
        "unbanned",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number),
    )
    .await
}

const REMOVE_ROLES_DOC: &str = "config-moderation-remove_roles_on_mute";
const RESTORE_ROLES_DOC: &str = "config-moderation-restore_roles_on_mute";

fn role_list_setting(description: &str) -> serde_json::Value {
    json!({
        "oneOf": [
            { "type": "boolean" },
            { "type": "array", "maxItems": 50, "items": { "type": "string", "pattern": "^[0-9]{1,20}$" } }
        ],
        "description": description,
        "x-ui": { "level": "basic", "widget": "flag-or-roles" }
    })
}

/// The settings a server can change: their schema, and their defaults.
#[allow(clippy::too_many_lines)]
fn settings() -> (serde_json::Value, serde_json::Value) {
    (
        json!({
            "type": "object",
            "properties": {
                "warn_expire_days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 365,
                    "description": "config-moderation-warn_expire_days",
                    "x-ui": { "level": "basic" }
                },
                "dm_on_action": {
                    "type": "boolean",
                    "description": "config-moderation-dm_on_action",
                    "x-ui": { "level": "basic" }
                }, "dm_messages": punish::dm_messages_schema("config-moderation-dm_messages"),
                "require_reason": {
                    "type": "boolean",
                    "description": "config-moderation-require_reason",
                    "x-ui": { "level": "basic" }
                },
                "ban_delete_days": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 7,
                    "description": "config-moderation-ban_delete_days",
                    "x-ui": { "level": "basic" }
                },
                "escalation_enabled": {
                    "type": "boolean",
                    "description": "config-moderation-escalation_enabled",
                    "x-ui": { "level": "basic" }
                },
                "manual_cases": {
                    "type": "boolean",
                    "description": "config-moderation-manual_cases",
                    "x-ui": { "level": "basic" }
                },
                "approval_channel": {
                    "type": ["string", "null"],
                    "pattern": "^[0-9]{1,20}$",
                    "description": "config-moderation-approval_channel",
                    "x-ui": { "level": "basic", "widget": "channel" }
                },
                "approval_bulk_from": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 300,
                    "description": "config-moderation-approval_bulk_from",
                    "x-ui": { "level": "basic" }
                },
                "approval_permanent_ban": {
                    "type": "boolean",
                    "description": "config-moderation-approval_permanent_ban",
                    "x-ui": { "level": "basic" }
                },
                "slowmode_exempt_roles": {
                    "type": "array",
                    "maxItems": 50,
                    "items": { "type": "string", "pattern": "^[0-9]{1,20}$" },
                    "description": "config-moderation-slowmode_exempt_roles",
                    "x-ui": { "level": "basic" }
                },
                "export_channel": {
                    "type": ["string", "null"],
                    "pattern": "^[0-9]{1,20}$",
                    "description": "config-moderation-export_channel",
                    "x-ui": { "level": "basic", "widget": "channel" }
                },
                "alert_on_rejoin": {
                    "type": "boolean",
                    "description": "config-moderation-alert_on_rejoin",
                    "x-ui": { "level": "basic" }
                },
                "alert_channel": {
                    "type": ["string", "null"],
                    "pattern": "^[0-9]{1,20}$",
                    "description": "config-moderation-alert_channel",
                    "x-ui": { "level": "basic", "widget": "channel" }
                },
                "templates_enabled": {
                    "type": "boolean",
                    "description": "config-moderation-templates_enabled",
                    "x-ui": { "level": "basic" }
                },
                "escalation": {
                    "type": "array",
                    "maxItems": 10,
                    "description": "config-moderation-escalation",
                    "x-ui": { "level": "basic" },
                    "items": {
                        "type": "object",
                        "properties": {
                            "warns": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 100,
                                "description": "config-moderation-warns"
                            },
                            "action": {
                                "type": "string",
                                "enum": ["timeout", "kick", "ban"]
                            },
                            "duration": {
                                "type": "string",
                                "pattern": "^([0-9]{1,10}[smhdw] ?)+$",
                                "description": "config-moderation-duration"
                            }
                        },
                        "required": ["warns", "action"],
                        "additionalProperties": false
                    }
                },
                "timeout_method": {
                    "type": "string",
                    "enum": ["discord", "role"],
                    "description": "config-moderation-timeout_method",
                    "x-ui": { "level": "basic" }
                },
                "timeout_role": {
                    "type": ["string", "null"],
                    "pattern": "^[0-9]{1,20}$",
                    "description": "config-moderation-timeout_role",
                    "x-ui": { "level": "basic", "widget": "role" }
                },
                "voice_mute_method": {
                    "type": "string",
                    "enum": ["discord", "role"],
                    "description": "config-moderation-voice_mute_method",
                    "x-ui": { "level": "basic" }
                },
                "voice_mute_role": {
                    "type": ["string", "null"],
                    "pattern": "^[0-9]{1,20}$",
                    "description": "config-moderation-voice_mute_role",
                    "x-ui": { "level": "basic", "widget": "role" }
                },
                "remove_roles_on_mute": role_list_setting(REMOVE_ROLES_DOC),
                "restore_roles_on_mute": role_list_setting(RESTORE_ROLES_DOC),
                "templates": templates::schema()
            },
            "additionalProperties": false
        }),
        json!({
            "warn_expire_days": 30,
            "dm_on_action": true, "dm_messages": {},
            "require_reason": false,
            "ban_delete_days": 0,
            "escalation": [
                {"warns": 3, "action": "timeout", "duration": "1d"},
                {"warns": 5, "action": "kick"}
            ],
            "timeout_method": "discord",
            "timeout_role": null,
            "voice_mute_method": "discord",
            "voice_mute_role": null,
            "escalation_enabled": true,
            "manual_cases": true,
            "export_channel": null,
            "approval_channel": null,
            "approval_bulk_from": 0,
            "approval_permanent_ban": false,
            "slowmode_exempt_roles": [],
            "alert_on_rejoin": false,
            "alert_channel": null,
            "templates_enabled": true,
            "remove_roles_on_mute": false,
            "restore_roles_on_mute": true,
            "templates": {}
        }),
    )
}

/// The commands that act on members, and the jobs that lift their punishments when the time is up.
fn punishments(builder: ModuleBuilder) -> ModuleBuilder {
    let builder = builder
        .command(
            Command::<TimeoutArgs>::new("timeout")
                .requires("moderation.timeout")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(timeout),
        )
        .command(
            Command::<MemberReason>::new("untimeout")
                .requires("moderation.timeout")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(untimeout),
        )
        .command(
            Command::<MemberReason>::new("kick")
                .requires("moderation.kick")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(kick),
        )
        .command(
            Command::<BanArgs>::new("ban")
                .requires("moderation.ban")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(ban),
        )
        .command(
            Command::<appeals::Appeal>::new("appeal")
                .requires("moderation.appeal")
                .handler(appeals::file),
        )
        .command(
            Command::<MemberReason>::new("unban")
                .requires("moderation.ban")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(unban),
        )
        .command(
            Command::<restrict::VoiceMute>::new("vcmute")
                .requires("moderation.voice")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(restrict::voice_mute),
        )
        .command(
            Command::<MemberReason>::new("vcunmute")
                .requires("moderation.voice")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(restrict::voice_unmute),
        );
    punish::jobs(builder)
}

/// The permission keys of the module, one for each kind of action a moderator takes.
#[allow(
    clippy::too_many_lines,
    reason = "a list of the permission keys, one after the other"
)]
fn permissions(builder: ModuleBuilder) -> Result<ModuleBuilder, CoreError> {
    Ok(builder
        .permission(PermissionSpec::new(
            "moderation.userinfo",
            Level::MODERATOR,
            "permission-moderation-userinfo",
        )?)
        .permission(PermissionSpec::new(
            "moderation.massban",
            Level::OWNER,
            "permission-moderation-massban",
        )?)
        .permission(PermissionSpec::new(
            "moderation.warn",
            Level::MODERATOR,
            "permission-moderation-warn",
        )?)
        .permission(PermissionSpec::new(
            "moderation.note",
            Level::MODERATOR,
            "permission-moderation-note",
        )?)
        .permission(PermissionSpec::new(
            "moderation.cases",
            Level::MODERATOR,
            "permission-moderation-cases",
        )?)
        .permission(PermissionSpec::new(
            "moderation.revoke",
            Level::MODERATOR,
            "permission-moderation-revoke",
        )?)
        .permission(PermissionSpec::new(
            "moderation.addcase",
            Level::MODERATOR,
            "permission-moderation-addcase",
        )?)
        .permission(PermissionSpec::new(
            "moderation.hidecase",
            Level::ADMIN,
            "permission-moderation-hidecase",
        )?)
        .permission(PermissionSpec::new(
            "moderation.deletecase",
            Level::ADMIN,
            "permission-moderation-deletecase",
        )?)
        .permission(PermissionSpec::new(
            "moderation.appeal",
            Level::GENERAL,
            "permission-moderation-appeal",
        )?)
        .permission(PermissionSpec::new(
            "moderation.mywarnings",
            Level::GENERAL,
            "permission-moderation-mywarnings",
        )?)
        .permission(PermissionSpec::new(
            "moderation.appeals",
            Level::MODERATOR,
            "permission-moderation-appeals",
        )?)
        .permission(PermissionSpec::new(
            "moderation.approve",
            Level::ADMIN,
            "permission-moderation-approve",
        )?)
        .permission(PermissionSpec::new(
            "moderation.export",
            Level::ADMIN,
            "permission-moderation-export",
        )?)
        .permission(PermissionSpec::new(
            "moderation.timeout",
            Level::MODERATOR,
            "permission-moderation-timeout",
        )?)
        .permission(PermissionSpec::new(
            "moderation.kick",
            Level::MODERATOR,
            "permission-moderation-kick",
        )?)
        .permission(PermissionSpec::new(
            "moderation.ban",
            Level::ADMIN,
            "permission-moderation-ban",
        )?)
        .permission(PermissionSpec::new(
            "moderation.voice",
            Level::MODERATOR,
            "permission-moderation-voice",
        )?)
        .permission(PermissionSpec::new(
            "moderation.clear",
            Level::MODERATOR,
            "permission-moderation-clear",
        )?)
        .permission(PermissionSpec::new(
            "moderation.slowmode",
            Level::MODERATOR,
            "permission-moderation-slowmode",
        )?)
        .permission(PermissionSpec::new(
            "moderation.lock",
            Level::MODERATOR,
            "permission-moderation-lock",
        )?))
}

fn moderator_tools(builder: ModuleBuilder) -> ModuleBuilder {
    let long = Arc::new(channel_controls::LongState::default());
    menu::register(builder)
        .job(bulk::JOB, bulk::step)
        .job(
            "channel_restore_slowmode",
            channel_controls::restore_slowmode,
        )
        .job("channel_restore_lock", channel_controls::restore_lock)
        .command(
            Command::<channel_controls::Slowmode>::new("slowmode")
                .requires("moderation.slowmode")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler({
                    let long = Arc::clone(&long);
                    move |cx, args| channel_controls::slowmode(Arc::clone(&long), cx, args)
                }),
        )
        .on(Subscription::new(EventKind::MessageCreated), {
            move |ctx, event| channel_controls::enforce_long_slowmode(Arc::clone(&long), ctx, event)
        })
        .command(
            Command::<channel_controls::Lock>::new("lock")
                .requires("moderation.lock")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(channel_controls::lock),
        )
        .command(
            Command::<channel_controls::Unlock>::new("unlock")
                .requires("moderation.lock")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(channel_controls::unlock),
        )
        .command(
            Command::<Target>::new("userinfo")
                .requires("moderation.userinfo")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(userinfo::userinfo),
        )
}

#[allow(
    clippy::too_many_lines,
    reason = "the declaration: each command and job in turn"
)]
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = settings();
    let builder = ModuleBuilder::new("moderation", "Moderation", env!("CARGO_PKG_VERSION"))?
        .capability(Capability::Cases)
        .capability(Capability::SendMessages)
        .capability(Capability::ReadGuild)
        .capability(Capability::SendDirectMessages)
        .capability(Capability::KickMembers)
        .capability(Capability::BanMembers)
        .capability(Capability::ReadBans)
        .capability(Capability::ReadAuditLog)
        .capability(Capability::TimeoutMembers)
        .capability(Capability::ManageRoles)
        .capability(Capability::ManageMessages)
        .capability(Capability::ReadMessages)
        .capability(Capability::MuteMembers)
        .capability(Capability::ManageChannels)
        .capability(Capability::Schedule)
        .capability(Capability::Storage)
        // A privileged intent: it must be switched on for the bot in the Discord developer portal,
        // or Discord refuses the gateway connection altogether.
        // Members for joins, leaves and timeouts; moderation for the bans.
        .intents(Intents::GUILD_MEMBERS | Intents::GUILD_MODERATION | Intents::GUILD_MESSAGES)
        .on(Subscription::new(EventKind::MemberJoined), rejoin::on_join)
        .on(Subscription::new(EventKind::BanAdded), manual::on_change)
        .on(Subscription::new(EventKind::BanRemoved), manual::on_change)
        .on(Subscription::new(EventKind::MemberLeft), manual::on_change)
        .on(
            Subscription::new(EventKind::TimeoutChanged),
            manual::on_change,
        )
        .on(Subscription::new(EventKind::ConfigChanged), config_changed)
        .on(
            Subscription::new(EventKind::ChannelCreated),
            channel_created,
        )
        .config(schema, defaults)
        .component(templates::ROUTE, templates::pick)
        .component(undo::ROUTE, undo::press)
        .component(bulk::ROUTE, bulk::press)
        .component(appeals::ROUTE, appeals::press)
        .command(
            Command::<Warn>::new("warn")
                .requires("moderation.warn")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(warn),
        )
        .command(
            Command::<Note>::new("note")
                .requires("moderation.note")
                .handler(note),
        )
        .command(
            Command::<Target>::new("warnings")
                .requires("moderation.cases")
                .handler(warnings),
        )
        .command(
            Command::<()>::new("mywarnings")
                .requires("moderation.mywarnings")
                .handler(mywarnings),
        )
        .command(
            Command::<clear::Clear>::new("clear")
                .requires("moderation.clear")
                .cooldown(Cooldown::per_user(Duration::from_secs(2)))
                .handler(clear::clear),
        )
        .group(
            "bulk",
            [
                Command::<bulk::Start>::new("start")
                    .requires("moderation.massban")
                    .handler(bulk::start),
                Command::<bulk::OfOperation>::new("status")
                    .requires("moderation.massban")
                    .handler(bulk::status),
                Command::<bulk::OfOperation>::new("cancel")
                    .requires("moderation.massban")
                    .handler(bulk::cancel),
                Command::<bulk::OfOperation>::new("retry")
                    .requires("moderation.massban")
                    .handler(bulk::retry),
            ],
        )
        .group(
            "case",
            [
                Command::<ViewArgs>::new("view")
                    .requires("moderation.cases")
                    .handler(case_view),
                Command::<Revoke>::new("revoke")
                    .requires("moderation.revoke")
                    .handler(case_revoke),
                Command::<casetools::Add>::new("add")
                    .requires("moderation.addcase")
                    .handler(casetools::add),
                Command::<casetools::Update>::new("update")
                    .requires("moderation.addcase")
                    .handler(casetools::update),
                Command::<casetools::Hide>::new("hide")
                    .requires("moderation.hidecase")
                    .handler(casetools::hide),
                Command::<casetools::Delete>::new("delete")
                    .requires("moderation.deletecase")
                    .handler(casetools::delete),
                Command::<casetools::Find>::new("search")
                    .requires("moderation.cases")
                    .handler(casetools::search),
            ],
        );
    Ok(permissions(punishments(moderator_tools(builder)))?
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA))
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod dm_config_tests {
    use super::Config;
    use serde_json::json;

    #[test]
    fn dm_templates_are_validated_on_read() {
        for text in ["{unknown}", "{reason", &"x".repeat(1501)] {
            assert!(
                serde_json::from_value::<Config>(json!({"dm_messages": {"warn": text}})).is_err()
            );
        }
    }

    #[test]
    fn a_dm_can_be_a_message_with_embeds() {
        let read = |value| serde_json::from_value::<Config>(json!({"dm_messages": {"ban": value}}));
        assert!(
            read(json!({"embeds": [{"title": "Banned from {server}", "description": "{reason}"}]}))
                .is_ok()
        );
        assert!(read(json!({"embeds": [{"title": "{nobody}"}]})).is_err());
        assert!(read(json!({"embeds": [{"colour": 1}]})).is_err());
        assert!(read(json!({})).is_err());
        assert!(read(serde_json::Value::Null).is_ok());
    }
}
