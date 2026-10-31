//! Replays the actions of a test run as messages on a test server (see `docs/preview.md`).
//!
//! The dumps are written by closure-core while `CLOSURE_DUMP_DIR` is set. Only the text of what
//! the bot says is posted, and without mentions: file names and buttons are dropped. Operations
//! that say nothing on their own (role changes, bans, timeouts, deletions) are counted instead.
//!
//! The token is `DISCORD_TOKEN`, read the way the bot reads it: from the environment, or from
//! `.env` when that file exists. The token is never printed.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use closure_core::action::{Action, InteractionReply, InteractionResponse, MentionPolicy};
use closure_core::client::DiscordApi;
use closure_core::event::{ChannelId, UserId};
use closure_discord::TwilightApi;
use serde_json::Value;
use twilight_http::Client;
use twilight_model::channel::message::Embed;
use twilight_model::id::Id;

/// The variable the bot reads its token from.
const TOKEN_VARIABLE: &str = "DISCORD_TOKEN";
const USAGE: &str = "usage: closure-xtask replay --dir <dir> --channel <channel id>";

/// What a replay would send, and what it leaves out.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// The messages to send, in the order of the dumps.
    pub posts: Vec<Action>,
    /// The actions that are not sent, counted by kind (`add_role`, `respond/defer`, ...).
    pub skipped: BTreeMap<String, usize>,
    /// Lines that repeat an earlier line exactly; each distinct action is sent once.
    pub duplicates: usize,
}

/// What one action becomes: a message to send, or the kind it is counted under.
enum Step {
    Send(Action),
    Skip(String),
}

/// Replays the dumps in `dir` to the channel `channel_id`. `Ok(false)` means that some messages
/// could not be sent.
///
/// # Errors
///
/// Fails before sending anything when the token is missing, the directory cannot be read, or a
/// line is not a recorded action.
pub fn run(dir: &Path, channel_id: u64) -> Result<bool> {
    // A missing `.env` is normal; variables already set in the environment win.
    let _ = dotenvy::dotenv();
    let token = std::env::var(TOKEN_VARIABLE)
        .ok()
        .filter(|value| !value.trim().is_empty());
    replay(dir, channel_id, token)
}

/// [`run`] with the token given instead of read from the environment.
fn replay(dir: &Path, channel_id: u64, token: Option<String>) -> Result<bool> {
    let Some(token) = token else {
        bail!("{TOKEN_VARIABLE} is not set (in the environment or in .env)\n{USAGE}");
    };
    let channel: ChannelId =
        Id::new_checked(channel_id).context("--channel must be a non-zero id")?;
    let lines = dump_lines(dir)?;
    let Plan {
        posts,
        skipped,
        duplicates,
    } = plan(lines.iter().map(String::as_str), channel)?;

    let total = posts.len();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    // Only messages are sent, and they do not use the application id (interaction routes do).
    let api = TwilightApi::new(Arc::new(Client::new(token)), Id::new(1));
    let failures = runtime.block_on(async {
        let mut failures = 0_usize;
        for (number, action) in posts.into_iter().enumerate() {
            if let Err(error) = api.execute(action).await {
                failures += 1;
                eprintln!("message {}: not sent: {error}", number + 1);
            }
        }
        failures
    });

    println!(
        "sent {} of {total} message(s); {duplicates} duplicate line(s) left out",
        total - failures
    );
    for (kind, count) in &skipped {
        println!("not sent, {kind}: {count}");
    }
    Ok(failures == 0)
}

/// The lines of every `.jsonl` dump in `dir`, file by file in name order. The order between
/// files is by process id, not by time.
fn dump_lines(dir: &Path) -> Result<Vec<String>> {
    let entries = fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
    files.sort();
    let mut lines = Vec::new();
    for file in &files {
        let text =
            fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
        lines.extend(
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned),
        );
    }
    Ok(lines)
}

/// Turns recorded action lines into the messages to send to `channel`.
///
/// # Errors
///
/// Fails on the first line that is not JSON or not a recorded action.
pub fn plan<'a>(lines: impl IntoIterator<Item = &'a str>, channel: ChannelId) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut seen = HashSet::new();
    for (index, line) in lines.into_iter().enumerate() {
        let number = index + 1;
        let value: Value =
            serde_json::from_str(line).with_context(|| format!("line {number}: not JSON"))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        let action: Action = serde_json::from_value(value)
            .with_context(|| format!("line {number}: not a recorded action"))?;
        if !seen.insert(serde_json::to_string(&action)?) {
            plan.duplicates += 1;
            continue;
        }
        match step(action, &kind, channel) {
            Step::Send(post) => plan.posts.push(post),
            Step::Skip(label) => *plan.skipped.entry(label).or_default() += 1,
        }
    }
    Ok(plan)
}

fn step(action: Action, kind: &str, channel: ChannelId) -> Step {
    match action {
        Action::SendMessage {
            content, embeds, ..
        }
        | Action::SendFile {
            content, embeds, ..
        } => message(channel, content, embeds, kind),
        Action::SendDirectMessage {
            user_id,
            content,
            embeds,
            ..
        } => message(channel, Some(direct_text(user_id, content)), embeds, kind),
        Action::Respond {
            response: InteractionResponse::Message { reply } | InteractionResponse::Update { reply },
            ..
        }
        | Action::Followup { reply, .. }
        | Action::EditResponse { reply, .. } => from_reply(channel, reply, kind),
        Action::Respond { response, .. } => {
            Step::Skip(format!("respond/{}", response_kind(&response)))
        }
        _ => Step::Skip(kind.to_owned()),
    }
}

/// The text of a direct message, under a first line that names the member it was meant for.
fn direct_text(user_id: UserId, content: Option<String>) -> String {
    match content {
        Some(text) => format!("DM to {user_id}\n{text}"),
        None => format!("DM to {user_id}"),
    }
}

/// The message of an interaction reply. Its buttons and select menus are not sent.
fn from_reply(channel: ChannelId, reply: InteractionReply, kind: &str) -> Step {
    message(channel, reply.content, reply.embeds, kind)
}

fn message(channel: ChannelId, content: Option<String>, embeds: Vec<Embed>, kind: &str) -> Step {
    if content.is_none() && embeds.is_empty() {
        return Step::Skip(format!("{kind}/empty"));
    }
    Step::Send(Action::SendMessage {
        channel_id: channel,
        content,
        embeds,
        reply_to: None,
        mentions: MentionPolicy::None,
    })
}

fn response_kind(response: &InteractionResponse) -> &'static str {
    match response {
        InteractionResponse::Defer { .. } => "defer",
        InteractionResponse::Message { .. } => "message",
        InteractionResponse::Update { .. } => "update",
        InteractionResponse::DeferUpdate => "defer_update",
        InteractionResponse::Modal(_) => "modal",
        InteractionResponse::Autocomplete { .. } => "autocomplete",
    }
}

#[cfg(test)]
mod tests {
    use closure_core::action::{Action, InteractionReply, InteractionResponse, Reason};
    use closure_core::event::{
        ChannelId, GuildId, InteractionId, InteractionToken, RoleId, UserId,
    };

    use super::{Plan, plan, replay};

    fn channel() -> ChannelId {
        ChannelId::new(100)
    }

    fn line(action: &Action) -> String {
        serde_json::to_string(action).unwrap()
    }

    fn sent(text: &str) -> Action {
        Action::SendMessage {
            channel_id: channel(),
            content: Some(text.to_owned()),
            embeds: vec![],
            reply_to: None,
            mentions: closure_core::action::MentionPolicy::None,
        }
    }

    fn plan_of(actions: &[Action]) -> Plan {
        let lines: Vec<String> = actions.iter().map(line).collect();
        plan(lines.iter().map(String::as_str), channel()).unwrap()
    }

    #[test]
    fn a_direct_message_is_posted_to_the_channel_under_a_header_line() {
        let dm = Action::SendDirectMessage {
            guild_id: GuildId::new(1),
            user_id: UserId::new(42),
            content: Some("hi there".to_owned()),
            embeds: vec![],
        };

        let plan = plan_of(&[dm]);

        assert_eq!(plan.posts, vec![sent("DM to 42\nhi there")]);
    }

    #[test]
    fn a_respond_with_a_message_is_posted() {
        let respond = Action::Respond {
            interaction_id: InteractionId::new(1),
            token: InteractionToken::new("token"),
            response: InteractionResponse::Message {
                reply: InteractionReply {
                    content: Some("pong".to_owned()),
                    ..Default::default()
                },
            },
        };

        let plan = plan_of(&[respond]);

        assert_eq!(plan.posts, vec![sent("pong")]);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn a_respond_that_only_defers_is_counted_not_posted() {
        let respond = Action::Respond {
            interaction_id: InteractionId::new(1),
            token: InteractionToken::new("token"),
            response: InteractionResponse::Defer { ephemeral: false },
        };

        let plan = plan_of(&[respond]);

        assert!(plan.posts.is_empty());
        assert_eq!(plan.skipped.get("respond/defer"), Some(&1));
    }

    #[test]
    fn a_role_change_is_counted_and_not_posted() {
        let grant = Action::AddRole {
            guild_id: GuildId::new(1),
            user_id: UserId::new(42),
            role_id: RoleId::new(5),
            reason: Reason::new("test").unwrap(),
        };

        let plan = plan_of(&[grant]);

        assert!(plan.posts.is_empty());
        assert_eq!(plan.skipped.get("add_role"), Some(&1));
    }

    #[test]
    fn the_same_line_twice_is_posted_once() {
        let plan = plan_of(&[sent("hello"), sent("hello")]);

        assert_eq!(plan.posts, vec![sent("hello")]);
        assert_eq!(plan.duplicates, 1);
    }

    #[test]
    fn a_message_with_nothing_in_it_is_counted_as_empty() {
        let empty = Action::SendMessage {
            channel_id: channel(),
            content: None,
            embeds: vec![],
            reply_to: None,
            mentions: closure_core::action::MentionPolicy::None,
        };

        let plan = plan_of(&[empty]);

        assert!(plan.posts.is_empty());
        assert_eq!(plan.skipped.get("send_message/empty"), Some(&1));
    }

    #[test]
    fn a_line_that_is_not_an_action_fails_the_whole_plan() {
        assert!(plan(["{not json"], channel()).is_err());
    }

    #[test]
    fn without_a_token_nothing_is_read_or_sent() {
        let missing = std::path::Path::new("/nonexistent-closure-dump-directory");

        let error = replay(missing, 100, None).unwrap_err();

        assert!(error.to_string().contains("DISCORD_TOKEN"));
    }
}
