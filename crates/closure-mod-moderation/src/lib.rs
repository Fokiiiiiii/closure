//! The `moderation` module: warnings, notes and the cases of a server.
//!
//! Written with the SDK only. It keeps no data of its own: every warning and note is a case of
//! the SDK's cases, numbered per server and shared by the modules. A warning counts until it is
//! revoked or its expiry is due, so nothing has to run later to expire one.

use std::time::Duration;

use closure_sdk::duration::{TimeStyle, timestamp};
use closure_sdk::embed::{escape, truncate};
use closure_sdk::modlog;
use closure_sdk::{
    Args, Capability, Case, CaseError, CaseKind, Cases, Command, CommandArgs, CommandCx, Cooldown,
    CoreError, DirectMessage, InteractionReply, Level, Locale, ModuleBuilder, NewCase,
    PermissionSpec, SdkModule, Sent, UserError, UserId,
};
use serde::Deserialize;
use serde_json::json;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

const DAY: i64 = 86_400;
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
struct Config {
    /// After how many days a warning expires.
    warn_expire_days: u32,
    /// Whether the warned member gets a direct message.
    dm_on_action: bool,
    /// Whether a warning needs a reason.
    require_reason: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            warn_expire_days: 30,
            dm_on_action: true,
            require_reason: false,
        }
    }
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

/// The one place that decides whether a case is a warning that still counts.
fn counts_as_warning(case: &Case, now: i64) -> bool {
    case.kind.as_str() == "warn" && state(case, now) == State::Active
}

/// The active warnings of `target`, newest first, at most `wanted`. Looks back over at most
/// [`SCAN`] cases.
async fn active_warnings(
    cases: &Cases,
    target: UserId,
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
        found.extend(
            page.iter()
                .filter(|case| counts_as_warning(case, now))
                .cloned(),
        );
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

/// Warns a member. The warning is a case that expires after `warn_expire_days`.
async fn warn(cx: CommandCx, args: Warn) -> Result<(), UserError> {
    cx.check_target(args.user).await?;
    let config: Config = cx.config()?;
    let reason = reason_of(args.reason);
    if config.require_reason && reason.is_none() {
        return Err(UserError::message("reason-required"));
    }
    let now = cx.now();
    let case = cx
        .cases()
        .create(NewCase {
            kind: CaseKind::new("warn")?,
            target: args.user,
            moderator: cx.user().id,
            reason: reason.clone(),
            expires_at: Some(now + i64::from(config.warn_expire_days) * DAY),
        })
        .await?;
    let active = active_warnings(cx.cases(), args.user, now, usize::MAX)
        .await?
        .len();

    // The warning is stored already: a member who cannot be messaged keeps it, and the reply says
    // that the message did not arrive instead of failing.
    let mut delivered = true;
    if config.dm_on_action {
        let reason_words = match reason.as_deref() {
            Some(text) => escape(text),
            None => cx.ctx().t_in(cx.ctx().locale, "reason-none", &Args::new()),
        };
        let text = cx.ctx().t_in(
            cx.ctx().locale,
            "dm-warned",
            &Args::new()
                .with("reason", reason_words)
                .with("active", count(active)),
        );
        delivered = matches!(
            cx.direct_message(args.user, DirectMessage::text(text))
                .await,
            Ok(Sent::Delivered(_))
        );
    }

    // A log that cannot take the case must not turn the warning into a failure.
    let _ = cx.modlog(&case).await;

    let mut answer = cx.t(
        "warned",
        &Args::new()
            .with("user", mention(args.user))
            .with("number", case.number)
            .with("active", count(active)),
    );
    if config.dm_on_action && !delivered {
        answer.push(' ');
        answer.push_str(&cx.t("dm-not-delivered", &Args::new()));
    }
    cx.reply_private(answer).await
}

/// Notes something about a member. A note is a case without expiry and is not a warning.
async fn note(cx: CommandCx, args: Note) -> Result<(), UserError> {
    let case = cx
        .cases()
        .create(NewCase {
            kind: CaseKind::new("note")?,
            target: args.user,
            moderator: cx.user().id,
            reason: Some(args.text),
            expires_at: None,
        })
        .await?;
    let _ = cx.modlog(&case).await;
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
    let found = active_warnings(cx.cases(), args.user, now, SHOWN).await?;
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

/// The module, still open for a test to build on.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("moderation", "Moderation", env!("CARGO_PKG_VERSION"))?
            .description(
                "Warn members, note about them, and look at and revoke the cases of a server.",
            )
            .capability(Capability::Cases)
            .capability(Capability::SendMessages)
            .capability(Capability::ReadGuild)
            .capability(Capability::SendDirectMessages)
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
            .config(
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
                        }
                    },
                    "additionalProperties": false
                }),
                json!({
                    "warn_expire_days": 30,
                    "dm_on_action": true,
                    "require_reason": false
                }),
            )
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
            )
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

/// The module as the host loads it.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}
