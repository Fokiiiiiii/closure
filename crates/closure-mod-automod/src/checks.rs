//! The checks of a rule: each reads one message and says whether it hits, and why.

use std::time::Duration;

use closure_sdk::text::{self, Link, MatchMode, WordList};
use closure_sdk::{Args, EventContext, MessageInfo, ModuleError};

use crate::config::{Check, LinkMode, Matching, Rule};

/// What a check found: a sentence in the server's language, for the log and the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub detail: String,
}

/// Runs the rule's check on `message`. Only a new message counts towards a flood, so `created` is
/// false for an edit.
pub async fn evaluate(
    ctx: &EventContext,
    rule: &Rule,
    message: &MessageInfo,
    created: bool,
) -> Result<Option<Hit>, ModuleError> {
    let content = message.content.as_str();
    let detail = match &rule.check {
        Check::Words { words, r#match } => {
            let mode = match r#match {
                Matching::Contains => MatchMode::Contains,
                Matching::Whole => MatchMode::Whole,
            };
            WordList::new(words, mode)
                .find(content)
                .map(|word| say(ctx, "detail-words", &Args::new().with("word", word)))
        }
        Check::Links { mode, domains } => link_detail(ctx, *mode, domains, content),
        Check::Invites { allow } => text::invites(content)
            .into_iter()
            .find(|code| !allow.contains(code))
            .map(|code| say(ctx, "detail-invite", &Args::new().with("code", code))),
        Check::Mentions { max } => {
            let count = wide(text::mention_count(message));
            (count > u64::from(*max))
                .then(|| say(ctx, "detail-mentions", &Args::new().with("count", count)))
        }
        Check::Caps {
            max_ratio,
            min_letters,
        } => text::caps_ratio(content, usize::try_from(*min_letters).unwrap_or(usize::MAX))
            .filter(|ratio| ratio > max_ratio)
            .map(|ratio| {
                let percent = format!("{:.0}%", ratio * 100.0);
                say(ctx, "detail-caps", &Args::new().with("percent", percent))
            }),
        Check::Repeat { max_run } => {
            let run = wide(text::longest_run(content));
            (run > u64::from(*max_run))
                .then(|| say(ctx, "detail-repeat", &Args::new().with("count", run)))
        }
        Check::Emoji { max } => {
            let count = wide(text::emoji_count(content));
            (count > u64::from(*max))
                .then(|| say(ctx, "detail-emoji", &Args::new().with("count", count)))
        }
        Check::Attachments {
            max,
            blocked_extensions,
        } => attachment_detail(ctx, message, *max, blocked_extensions),
        Check::Flood {
            messages,
            per_minutes,
        } => {
            if !created {
                return Ok(None);
            }
            flood(
                ctx,
                &rule.id,
                message.author.id.get(),
                *messages,
                *per_minutes,
            )
            .await?
        }
    };
    Ok(detail.map(|detail| Hit { detail }))
}

fn link_detail(
    ctx: &EventContext,
    mode: LinkMode,
    domains: &[String],
    content: &str,
) -> Option<String> {
    let links = text::links(content);
    let listed = |link: &Link| domains.iter().any(|domain| link.is_domain(domain));
    match mode {
        LinkMode::Any => links
            .first()
            .map(|link| say(ctx, "detail-link", &host(link))),
        LinkMode::Allowlist => links
            .iter()
            .find(|link| !listed(link))
            .map(|link| say(ctx, "detail-link-not-allowed", &host(link))),
        LinkMode::Blocklist => links
            .iter()
            .find(|link| listed(link))
            .map(|link| say(ctx, "detail-link-blocked", &host(link))),
    }
}

fn attachment_detail(
    ctx: &EventContext,
    message: &MessageInfo,
    max: Option<u32>,
    blocked: &[String],
) -> Option<String> {
    let count = wide(message.attachments.len());
    match max {
        Some(max) if count > u64::from(max) => Some(say(
            ctx,
            "detail-attachments",
            &Args::new().with("count", count),
        )),
        _ => message
            .attachments
            .iter()
            .find(|file| is_blocked(&file.filename, blocked))
            .map(|file| {
                let args = Args::new().with("name", file.filename.as_str());
                say(ctx, "detail-attachment-type", &args)
            }),
    }
}

/// Counts the message and tells whether the burst has reached its size. The counter is the rule's,
/// per author.
async fn flood(
    ctx: &EventContext,
    rule_id: &str,
    author: u64,
    messages: u32,
    per_minutes: u32,
) -> Result<Option<String>, ModuleError> {
    let store = ctx.store();
    store
        .counter_add(rule_id, author, 1)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let window = Duration::from_secs(u64::from(per_minutes) * 60);
    let recent = store
        .counter_recent(rule_id, author, window)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok((recent >= i64::from(messages)).then(|| {
        let args = Args::new()
            .with("count", recent)
            .with("minutes", i64::from(per_minutes));
        say(ctx, "detail-flood", &args)
    }))
}

/// The name a check gives its kind, as the status and the dry run show it.
pub fn kind_key(check: &Check) -> &'static str {
    match check {
        Check::Words { .. } => "check-words",
        Check::Links { .. } => "check-links",
        Check::Invites { .. } => "check-invites",
        Check::Mentions { .. } => "check-mentions",
        Check::Caps { .. } => "check-caps",
        Check::Repeat { .. } => "check-repeat",
        Check::Emoji { .. } => "check-emoji",
        Check::Attachments { .. } => "check-attachments",
        Check::Flood { .. } => "check-flood",
    }
}

fn say(ctx: &EventContext, key: &str, args: &Args) -> String {
    ctx.t_in(ctx.locale, key, args)
}

fn host(link: &Link) -> Args {
    Args::new().with("host", link.host.as_str())
}

/// A count as the wide integer the messages use; a count never reaches the limit of `u64`.
fn wide(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// Whether the file's extension is one of the blocked ones, in any case.
fn is_blocked(filename: &str, blocked: &[String]) -> bool {
    filename.rsplit_once('.').is_some_and(|(_, extension)| {
        blocked.iter().any(|listed| {
            listed
                .trim_start_matches('.')
                .eq_ignore_ascii_case(extension)
        })
    })
}
