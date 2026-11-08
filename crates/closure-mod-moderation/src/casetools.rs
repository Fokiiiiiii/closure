//! The tools for the cases themselves: record one by hand, change its reason, hide it from the
//! lists, delete it, and find cases by who, what, when and how. The search can also be written out
//! as a CSV file for the staff.

use std::fmt::Write as _;

use closure_core::action::{Action, FileUpload};
use closure_core::cases::{
    Case, CaseEdit, CaseFilter, CaseKind, CaseSource, CaseState, MAX_SEARCH, NewCase,
};
use closure_sdk::duration::parse as parse_duration;
use closure_sdk::{Args, CommandArgs, CommandCx, Tone, UserError, UserId};

use crate::{Config, listed_reason, mention, not_found, reason_of, state};

const PAGE: usize = 10;
/// The kinds a case recorded by hand may be of. A lift (`unban`, `untimeout`, `vcunmute`) is only
/// written down: it undoes nothing, as the commands that lift do.
const KINDS: [&str; 10] = [
    "note",
    "warn",
    "kick",
    "ban",
    "timeout",
    "softban",
    "vcmute",
    "unban",
    "untimeout",
    "vcunmute",
];

#[derive(CommandArgs)]
pub(crate) struct Add {
    /// Member
    user: UserId,
    /// What it is: note, warn, kick, ban, timeout, softban, vcmute, unban, untimeout or vcunmute
    kind: String,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
pub(crate) struct Update {
    /// Number of the case
    number: i64,
    /// The new reason
    #[arg(max_length = 512)]
    reason: Option<String>,
    /// The moderator to name for the case
    moderator: Option<UserId>,
}

#[derive(CommandArgs)]
pub(crate) struct Hide {
    /// Number of the case
    number: i64,
    /// Yes: show the case again instead of hiding it
    show: Option<bool>,
}

#[derive(CommandArgs)]
pub(crate) struct Delete {
    /// Number of the case
    number: i64,
    /// Yes: the case is gone for good
    confirm: bool,
}

/// What a search asks for; the same for the search and for its export.
#[derive(CommandArgs)]
pub(crate) struct Find {
    /// Cases against this member
    user: Option<UserId>,
    /// Cases made by this moderator
    moderator: Option<UserId>,
    /// The kind: warn, note, ban, kick, timeout, softban, ...
    #[arg(max_length = 32)]
    kind: Option<String>,
    /// Where it came from: command, button, automod, manual, dashboard, script, appeal
    #[arg(max_length = 16)]
    source: Option<String>,
    /// Where it stands: active, expired or revoked
    #[arg(max_length = 16)]
    state: Option<String>,
    /// Made within this long, such as 7d
    #[arg(max_length = 32)]
    within: Option<String>,
    /// A word of the reason
    #[arg(max_length = 100)]
    text: Option<String>,
    /// Include hidden cases (needs the permission to hide cases)
    hidden: Option<bool>,
    /// Show only the cases numbered below this
    before: Option<i64>,
    /// Yes: send the result as a CSV file to the export channel (needs the permission to export)
    export: Option<bool>,
}

fn kind_of(text: &str) -> Result<CaseKind, UserError> {
    CaseKind::new(text.trim().to_lowercase().as_str())
        .map_err(|_| UserError::message("case-kind-unknown"))
}

/// Records a case that no command made: something that happened elsewhere, or a note.
pub(crate) async fn add(cx: CommandCx, args: Add) -> Result<(), UserError> {
    let kind = args.kind.trim().to_lowercase();
    if !KINDS.contains(&kind.as_str()) {
        return Err(UserError::message("case-kind-unknown"));
    }
    // A recorded ban or timeout counts as a standing punishment, so it takes the permission of the
    // punishment itself and not only the one to add cases.
    cx.require(match kind.as_str() {
        "ban" | "softban" | "unban" => "moderation.ban",
        "kick" => "moderation.kick",
        "timeout" | "untimeout" => "moderation.timeout",
        "vcmute" | "vcunmute" => "moderation.voice",
        "warn" => "moderation.warn",
        _ => "moderation.note",
    })
    .await?;
    let case = cx
        .cases()
        .create(NewCase {
            source: CaseSource::Command,
            kind: kind_of(&kind)?,
            target: args.user,
            moderator: cx.user().id,
            reason: reason_of(args.reason),
            expires_at: None,
        })
        .await?;
    let _ = cx.modlog(&case).await;
    cx.say_private_t(
        Tone::Success,
        "case-added",
        &Args::new()
            .with("number", case.number)
            .with("user", mention(args.user)),
    )
    .await
}

pub(crate) async fn update(cx: CommandCx, args: Update) -> Result<(), UserError> {
    let reason = reason_of(args.reason);
    if reason.is_none() && args.moderator.is_none() {
        return Err(UserError::message("case-update-nothing"));
    }
    let case = cx
        .cases()
        .edit(
            args.number,
            CaseEdit {
                reason,
                hidden: None,
                moderator: args.moderator,
            },
        )
        .await
        .map_err(|error| match error {
            closure_core::cases::CaseError::NotFound => not_found(args.number),
            other => other.into(),
        })?;
    let _ = cx.modlog(&case).await;
    cx.say_private_t(
        Tone::Success,
        "case-updated",
        &Args::new().with("number", case.number),
    )
    .await
}

async fn hide_or_show(cx: &CommandCx, number: i64, hidden: bool) -> Result<(), UserError> {
    cx.cases()
        .edit(
            number,
            CaseEdit {
                hidden: Some(hidden),
                ..CaseEdit::default()
            },
        )
        .await
        .map_err(|error| match error {
            closure_core::cases::CaseError::NotFound => not_found(number),
            other => other.into(),
        })?;
    cx.say_private_t(
        Tone::Success,
        if hidden {
            "case-hidden"
        } else {
            "case-unhidden"
        },
        &Args::new().with("number", number),
    )
    .await
}

pub(crate) async fn hide(cx: CommandCx, args: Hide) -> Result<(), UserError> {
    hide_or_show(&cx, args.number, !args.show.unwrap_or(false)).await
}

pub(crate) async fn delete(cx: CommandCx, args: Delete) -> Result<(), UserError> {
    if !args.confirm {
        return Err(UserError::message("case-delete-confirm").with("number", args.number));
    }
    if !cx.cases().delete(args.number).await? {
        return Err(not_found(args.number));
    }
    cx.say_private_t(
        Tone::Success,
        "case-deleted",
        &Args::new().with("number", args.number),
    )
    .await
}

async fn filter_of(cx: &CommandCx, find: &Find) -> Result<CaseFilter, UserError> {
    let source = find
        .source
        .as_deref()
        .map(|text| {
            CaseSource::named(&text.trim().to_lowercase())
                .ok_or_else(|| UserError::message("case-source-unknown"))
        })
        .transpose()?;
    let state = find
        .state
        .as_deref()
        .map(|text| {
            CaseState::named(&text.trim().to_lowercase())
                .ok_or_else(|| UserError::message("case-state-unknown"))
        })
        .transpose()?;
    let since = find
        .within
        .as_deref()
        .map(|text| {
            parse_duration(text)
                .map(|span| cx.now() - i64::try_from(span.as_secs()).unwrap_or(i64::MAX))
                .map_err(|_| UserError::message("case-window-bad"))
        })
        .transpose()?;
    let include_hidden = find.hidden.unwrap_or(false);
    if include_hidden {
        cx.require("moderation.hidecase").await?;
    }
    Ok(CaseFilter {
        target: find.user,
        moderator: find.moderator,
        kind: find
            .kind
            .as_deref()
            .map(|text| kind_of(text).map(|kind| kind.as_str().to_owned()))
            .transpose()?,
        source,
        state,
        since,
        until: None,
        text: find.text.clone().filter(|text| !text.trim().is_empty()),
        include_hidden,
    })
}

pub(crate) async fn search(cx: CommandCx, args: Find) -> Result<(), UserError> {
    if args.export == Some(true) {
        cx.require("moderation.export").await?;
        return export(cx, args).await;
    }
    let filter = filter_of(&cx, &args).await?;
    let now = cx.now();
    // One more than a page: whether there is a next one.
    let mut found = cx
        .cases()
        .search(&filter, now, args.before, PAGE + 1)
        .await?;
    if found.is_empty() {
        return cx.reply_private_t("case-search-none", &Args::new()).await;
    }
    let more = found.len() > PAGE;
    found.truncate(PAGE);
    let mut text = cx.t("case-search-header", &Args::new());
    for case in &found {
        text.push('\n');
        text.push_str(
            &cx.t(
                "case-search-line",
                &Args::new()
                    .with("number", case.number)
                    .with("kind", case.kind.as_str().to_owned())
                    .with("source", case.source.as_str().to_owned())
                    .with("state", cx.t(state(case, now).key(), &Args::new()))
                    .with("target", mention(case.target))
                    .with("moderator", mention(case.moderator))
                    .with("reason", listed_reason(&cx, case.reason.as_deref())),
            ),
        );
    }
    if more && let Some(last) = found.last() {
        text.push('\n');
        text.push_str(&cx.t("case-search-more", &Args::new().with("number", last.number)));
    }
    cx.reply_private(text).await
}

/// A cell of a CSV file. A text that a spreadsheet would run as a formula is made plain text.
fn cell(text: &str) -> String {
    let mut text = text.replace(['\r', '\n'], " ");
    if text.starts_with(['=', '+', '-', '@', '\t']) {
        text.insert(0, '\'');
    }
    if text.contains([',', '"']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text
    }
}

pub(crate) fn csv(cases: &[Case], now: i64) -> String {
    let mut out = String::from(
        "number,kind,target,moderator,source,state,created_at,expires_at,revoked_by,revoked_reason,hidden,reason\n",
    );
    for case in cases {
        let state = [CaseState::Revoked, CaseState::Expired, CaseState::Active]
            .into_iter()
            .find(|state| state.holds(case, now))
            .map_or("", CaseState::as_str);
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{}",
            case.number,
            cell(case.kind.as_str()),
            case.target,
            case.moderator,
            case.source.as_str(),
            state,
            case.created_at,
            case.expires_at.map_or(String::new(), |at| at.to_string()),
            case.revoked
                .as_ref()
                .map_or(String::new(), |revoked| revoked.by.to_string()),
            cell(
                case.revoked
                    .as_ref()
                    .and_then(|revoked| revoked.reason.as_deref())
                    .unwrap_or("")
            ),
            case.hidden,
            cell(case.reason.as_deref().unwrap_or("")),
        );
    }
    out
}

async fn export(cx: CommandCx, args: Find) -> Result<(), UserError> {
    cx.defer(true).await?;
    let config: Config = cx.config()?;
    let Some(channel) = config.export_target() else {
        return Err(UserError::message("case-export-no-channel"));
    };
    let filter = filter_of(&cx, &args).await?;
    let now = cx.now();
    let found = cx
        .cases()
        .search(&filter, now, args.before, MAX_SEARCH)
        .await?;
    if found.is_empty() {
        return cx.reply_private_t("case-search-none", &Args::new()).await;
    }
    let file = FileUpload {
        filename: format!("cases-{now}.csv"),
        data: csv(&found, now).into_bytes(),
    };
    cx.api()
        .execute(Action::SendFile {
            channel_id: channel,
            content: Some(
                cx.t(
                    "case-export-text",
                    &Args::new()
                        .with("count", found.len() as u64)
                        .with("moderator", mention(cx.user().id)),
                ),
            ),
            embeds: Vec::new(),
            file,
        })
        .await?;
    cx.say_private_t(
        Tone::Success,
        "case-export-sent",
        &Args::new()
            .with("count", found.len() as u64)
            .with("channel", format!("<#{channel}>")),
    )
    .await
}
