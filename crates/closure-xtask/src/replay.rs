//! Replays the showcase of a test run as messages on a test server (see `docs/preview.md`).
//!
//! The dumps are written by closure-core while `CLOSURE_DUMP_DIR` is set. Only the tests whose names
//! start with `showcase::` make the showcase, and the messages they produce are posted as they are,
//! without mentions. Direct messages are posted under a header line that names the member. Messages
//! that repeat a kind already posted, that are too short to read, or that carry the test fixtures' ids
//! are left out, as are the actions that say nothing on their own (role changes, bans, timeouts,
//! deletions).
//!
//! The token is `DISCORD_TOKEN`, read the way the bot reads it: from the environment, or from
//! `.env` when that file exists. The token is never printed.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use closure_core::action::{
    Action, FileUpload, InteractionReply, InteractionResponse, MentionPolicy,
};
use closure_core::client::DiscordApi;
use closure_core::event::{ChannelId, UserId};
use closure_core::locale::{Args, Locale, Translator};
use closure_discord::TwilightApi;
use closure_i18n::{CORE_NAMESPACE, TranslatorBuilder};
use serde_json::Value;
use twilight_http::Client;
use twilight_model::channel::message::Embed;
use twilight_model::id::Id;

/// The variable the bot reads its token from.
pub(crate) const TOKEN_VARIABLE: &str = "DISCORD_TOKEN";
/// The name prefix of the tests that make the showcase; a test binary does not name its tests by
/// module, so the prefix is in the function name.
const SHOWCASE_PREFIX: &str = "showcase_";
/// How much of a message's opening tells its kind apart from another message of the same kind.
const TEMPLATE_CHARS: usize = 25;
/// Messages with less text than this are left out: they are progress lines or test values.
const MIN_TEXT_CHARS: usize = 15;
/// The ids and names that only the test fixtures use.
const FIXTURE_MARKERS: [&str; 4] = ["<@7>", "<@9999>", "(7)", "(9999)"];
const USAGE: &str = "usage: closure-xtask replay --dir <dir> --channel <channel id>";

/// What a replay would send, and what it leaves out.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    /// The messages to send, in the order of the dumps.
    pub posts: Vec<Action>,
    /// Actions that are not messages, counted by kind (`add_role`, `respond/defer`, ...).
    pub skipped: BTreeMap<String, usize>,
    /// Actions of tests outside the showcase.
    pub outside: usize,
    /// Messages of a kind that an earlier message already showed (see `template_key`).
    pub similar: usize,
    /// Messages with too little text to read.
    pub short: usize,
    /// Messages that carry the test fixtures' ids (see `FIXTURE_MARKERS`).
    pub fixtures: usize,
}

/// What one action becomes.
enum Step {
    /// A message, with the key of its kind.
    Send { post: Box<Action>, key: String },
    /// A message with too little text to read.
    Short,
    /// A message that carries the test fixtures' ids.
    Fixture,
    /// An action that is not a message, counted under this label.
    Skip(String),
}

/// Replays the dumps in `dir` to the channel `channel_id`. `Ok(false)` means that some messages
/// could not be sent.
///
/// # Errors
///
/// Fails before sending anything when the directory cannot be read, a line is not a recorded
/// action, or the token is missing.
pub fn run(dir: &Path, channel_id: u64) -> Result<bool> {
    replay(dir, channel_id, token_from_env())
}

/// The token from the environment, or from `.env` when the variable is not set there. A missing
/// `.env` is normal.
pub(crate) fn token_from_env() -> Option<String> {
    let _ = dotenvy::dotenv();
    std::env::var(TOKEN_VARIABLE)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// [`run`] with the token given instead of read from the environment.
fn replay(dir: &Path, channel_id: u64, token: Option<String>) -> Result<bool> {
    let channel: ChannelId =
        Id::new_checked(channel_id).context("--channel must be a non-zero id")?;
    let lines = dump_lines(dir)?;
    let Plan {
        posts,
        skipped,
        outside,
        similar,
        short,
        fixtures,
    } = plan(lines.iter().map(String::as_str), channel)?;
    let Some(token) = token else {
        bail!("{TOKEN_VARIABLE} is not set (in the environment or in .env)\n{USAGE}");
    };

    let total = posts.len();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let failures = runtime.block_on(async move {
        // The client starts its rate limiter on the runtime it runs in, so it is made here.
        // Only messages are sent, and they do not use the application id (interaction routes do).
        let api = TwilightApi::new(Arc::new(Client::new(token)), Id::new(1));
        let mut failures = 0_usize;
        for (number, action) in posts.into_iter().enumerate() {
            if let Err(error) = api.execute(action).await {
                failures += 1;
                eprintln!("message {}: not sent: {error}", number + 1);
            }
        }
        failures
    });

    let not_messages: usize = skipped.values().sum();
    println!(
        "sent {} of {total} message(s); left out {outside} outside the showcase, {similar} similar, \
         {short} too short, {fixtures} with test ids, {not_messages} that are not messages",
        total - failures
    );
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
    let translator = TranslatorBuilder::new()?.build();
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
        let test = value
            .get("test")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let action: Action = serde_json::from_value(value)
            .with_context(|| format!("line {number}: not a recorded action"))?;
        if !test
            .rsplit("::")
            .next()
            .is_some_and(|name| name.starts_with(SHOWCASE_PREFIX))
        {
            plan.outside += 1;
            continue;
        }
        match step(action, &kind, channel, &translator) {
            Step::Send { post, key } => {
                if seen.insert(key) {
                    plan.posts.push(*post);
                } else {
                    plan.similar += 1;
                }
            }
            Step::Short => plan.short += 1,
            Step::Fixture => plan.fixtures += 1,
            Step::Skip(label) => *plan.skipped.entry(label).or_default() += 1,
        }
    }
    Ok(plan)
}

/// The key of a message's kind: the opening of its text with every number masked, so that a
/// warning or a ban is sent once however many numbers it names. A message without text is keyed by
/// its whole content, so that different embeds are never merged.
fn template_key(content: Option<&str>, embeds: &[Embed]) -> String {
    match text_of(content, embeds) {
        Some(text) => mask_numbers(&text).chars().take(TEMPLATE_CHARS).collect(),
        None => format!("{embeds:?}"),
    }
}

/// The text a message shows first: its content, or else the first embed's description or last field.
fn text_of(content: Option<&str>, embeds: &[Embed]) -> Option<String> {
    content
        .map(str::to_owned)
        .or_else(|| embeds.iter().find_map(embed_text))
}

/// The text an embed shows: its description, or else the value of its last field.
fn embed_text(embed: &Embed) -> Option<String> {
    embed
        .description
        .clone()
        .or_else(|| embed.fields.last().map(|field| field.value.clone()))
}

/// Everything a message shows, as one text: its content, and the titles, descriptions and fields of
/// its embeds.
fn all_text(content: Option<&str>, embeds: &[Embed]) -> String {
    let mut parts: Vec<&str> = content.into_iter().collect();
    for embed in embeds {
        parts.extend(embed.title.as_deref());
        parts.extend(embed.description.as_deref());
        for field in &embed.fields {
            parts.push(field.name.as_str());
            parts.push(field.value.as_str());
        }
    }
    parts.join("\n")
}

/// The text with every run of digits replaced by one `#`.
fn mask_numbers(text: &str) -> String {
    let mut masked = String::with_capacity(text.len());
    let mut in_digits = false;
    for character in text.chars() {
        if character.is_ascii_digit() {
            if !in_digits {
                masked.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            masked.push(character);
        }
    }
    masked
}

fn step(action: Action, kind: &str, channel: ChannelId, translator: &dyn Translator) -> Step {
    match action {
        Action::SendMessage {
            content, embeds, ..
        } => message(channel, content, embeds, kind),
        Action::SendFile {
            content,
            embeds,
            file,
            ..
        } => match message(channel, content, embeds, kind) {
            Step::Send { post, key } => Step::Send {
                post: Box::new(with_file(*post, file)),
                key,
            },
            other => other,
        },
        Action::SendDirectMessage {
            user_id,
            content,
            embeds,
            ..
        } => direct_message(
            channel,
            user_id,
            content.as_deref(),
            embeds,
            kind,
            translator,
        ),
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

/// A direct message, posted under a first line that names the member it was meant for. The line is
/// not what tells the kinds apart: the same message goes to many members, so the key is the text
/// the member reads. A message with text is checked with its line, as it always was; one without
/// text is checked by its embeds alone.
fn direct_message(
    channel: ChannelId,
    user_id: UserId,
    body: Option<&str>,
    embeds: Vec<Embed>,
    kind: &str,
    translator: &dyn Translator,
) -> Step {
    // The showcase this replays is in Japanese, so its header is too.
    let header = translator.translate(
        Locale::Ja,
        CORE_NAMESPACE,
        "replay-direct-message",
        &Args::new().with("user", user_id.to_string()),
    );
    let shown = match body {
        Some(text) => format!("{header}\n{text}"),
        None => header,
    };
    let key = template_key(body, &embeds);
    let checked = body.map(|_| shown.clone());
    match message(channel, checked, embeds, kind) {
        Step::Send { mut post, .. } => {
            if let Action::SendMessage { content, .. } = &mut *post {
                *content = Some(shown);
            }
            Step::Send { post, key }
        }
        other => other,
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
    let everything = all_text(content.as_deref(), &embeds);
    if FIXTURE_MARKERS
        .iter()
        .any(|marker| everything.contains(marker))
    {
        return Step::Fixture;
    }
    if text_of(content.as_deref(), &embeds)
        .is_some_and(|text| text.chars().count() < MIN_TEXT_CHARS)
    {
        return Step::Short;
    }
    let key = template_key(content.as_deref(), &embeds);
    Step::Send {
        post: Box::new(Action::SendMessage {
            channel_id: channel,
            content,
            embeds,
            reply_to: None,
            mentions: MentionPolicy::None,
        }),
        key,
    }
}

/// The message with the file it carries, so that the replay posts the file as the bot does.
fn with_file(post: Action, file: FileUpload) -> Action {
    match post {
        Action::SendMessage {
            channel_id,
            content,
            embeds,
            ..
        } => Action::SendFile {
            channel_id,
            content,
            embeds,
            file,
        },
        other => other,
    }
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
    use std::fs;

    use closure_core::action::{
        Action, FileUpload, InteractionReply, InteractionResponse, MentionPolicy, Reason,
    };
    use closure_core::event::{
        ChannelId, GuildId, InteractionId, InteractionToken, RoleId, UserId,
    };
    use serde_json::json;
    use twilight_model::channel::message::Embed;

    use super::{Plan, plan, replay};

    fn channel() -> ChannelId {
        ChannelId::new(100)
    }

    /// The line a showcase test records for `action`.
    fn line(action: &Action) -> String {
        let mut value = serde_json::to_value(action).unwrap();
        value["test"] = json!("showcase_example");
        value.to_string()
    }

    fn sent(text: &str) -> Action {
        Action::SendMessage {
            channel_id: channel(),
            content: Some(text.to_owned()),
            embeds: vec![],
            reply_to: None,
            mentions: MentionPolicy::None,
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

        assert_eq!(plan.posts, vec![sent("DM の宛先: 42\nhi there")]);
    }

    #[test]
    fn direct_messages_with_other_texts_are_all_posted_whoever_they_are_for() {
        let described = |text: &str| {
            serde_json::from_value::<Embed>(json!({ "type": "rich", "description": text })).unwrap()
        };
        let dm = |user: u64, text: &str| Action::SendDirectMessage {
            guild_id: GuildId::new(1),
            user_id: UserId::new(user),
            content: None,
            embeds: vec![described(text)],
        };

        let plan = plan_of(&[
            dm(42, "You were warned in a server. Reason: spam"),
            dm(43, "You were banned from a server. Reason: raid"),
        ]);

        assert_eq!(plan.similar, 0);
        assert_eq!(plan.posts.len(), 2);
        assert!(
            matches!(&plan.posts[0], Action::SendMessage { content: Some(text), .. } if text == "DM の宛先: 42"),
            "{:?}",
            plan.posts[0]
        );
    }

    #[test]
    fn a_respond_with_a_message_is_posted() {
        let respond = Action::Respond {
            interaction_id: InteractionId::new(1),
            token: InteractionToken::new("token"),
            response: InteractionResponse::Message {
                reply: InteractionReply {
                    content: Some("Pong! The bot is up".to_owned()),
                    ..Default::default()
                },
            },
        };

        let plan = plan_of(&[respond]);

        assert_eq!(plan.posts, vec![sent("Pong! The bot is up")]);
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
    fn messages_that_differ_only_in_numbers_are_posted_once() {
        let plan = plan_of(&[
            sent("Muted for 10 minutes in voice"),
            sent("Muted for 20 minutes in voice"),
            sent("Muted for 10 minutes in voice"),
        ]);

        assert_eq!(plan.posts, vec![sent("Muted for 10 minutes in voice")]);
        assert_eq!(plan.similar, 2);
    }

    #[test]
    fn messages_with_other_words_are_all_posted() {
        let plan = plan_of(&[
            sent("Muted for 10 minutes in voice"),
            sent("Banned for 10 minutes in voice"),
        ]);

        assert_eq!(plan.posts.len(), 2);
        assert_eq!(plan.similar, 0);
    }

    #[test]
    fn messages_that_open_the_same_way_are_posted_once() {
        let plan = plan_of(&[
            sent("You were warned in a server. Reason: spam"),
            sent("You were warned in a server. Reason: raid"),
        ]);

        assert_eq!(plan.posts.len(), 1);
        assert_eq!(plan.similar, 1);
    }

    #[test]
    fn messages_without_text_are_told_apart_by_their_embeds() {
        let embed = |title: &str| {
            serde_json::from_value::<Embed>(json!({ "type": "rich", "title": title })).unwrap()
        };
        let card = |title: &str| Action::SendMessage {
            channel_id: channel(),
            content: None,
            embeds: vec![embed(title)],
            reply_to: None,
            mentions: MentionPolicy::None,
        };

        let plan = plan_of(&[card("a"), card("b")]);

        assert_eq!(plan.posts.len(), 2);
    }

    #[test]
    fn a_message_with_nothing_in_it_is_counted_as_empty() {
        let empty = Action::SendMessage {
            channel_id: channel(),
            content: None,
            embeds: vec![],
            reply_to: None,
            mentions: MentionPolicy::None,
        };

        let plan = plan_of(&[empty]);

        assert!(plan.posts.is_empty());
        assert_eq!(plan.skipped.get("send_message/empty"), Some(&1));
    }

    #[test]
    fn a_message_that_carries_a_fixture_id_is_left_out() {
        let plan = plan_of(&[sent("You were warned <@7> for spam")]);

        assert!(plan.posts.is_empty());
        assert_eq!(plan.fixtures, 1);
    }

    #[test]
    fn a_short_message_is_left_out() {
        let plan = plan_of(&[sent("hi")]);

        assert!(plan.posts.is_empty());
        assert_eq!(plan.short, 1);
    }

    #[test]
    fn only_the_showcase_is_posted() {
        let mut other = serde_json::to_value(sent("Banned for 10 minutes in voice")).unwrap();
        other["test"] = json!("client::tests::example");
        let showcase = line(&sent("Muted for 10 minutes in voice"));

        let plan = plan([other.to_string().as_str(), showcase.as_str()], channel()).unwrap();

        assert_eq!(plan.posts, vec![sent("Muted for 10 minutes in voice")]);
        assert_eq!(plan.outside, 1);
    }

    #[test]
    fn a_file_is_posted_as_a_file_with_its_embed() {
        let embed = serde_json::from_value::<Embed>(
            json!({ "type": "rich", "description": "A picture of a message that was deleted." }),
        )
        .unwrap();
        let picture = Action::SendFile {
            channel_id: channel(),
            content: None,
            embeds: vec![embed],
            file: FileUpload {
                filename: "photo.png".to_owned(),
                data: vec![1, 2, 3],
            },
        };

        let plan = plan_of(std::slice::from_ref(&picture));

        assert_eq!(plan.posts, vec![picture]);
    }

    #[test]
    fn a_line_that_is_not_an_action_fails_the_whole_plan() {
        assert!(plan(["{not json"], channel()).is_err());
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("closure-replay-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn without_a_token_nothing_is_sent() {
        let dir = test_dir("without-token");

        let error = replay(&dir, 100, None).unwrap_err();
        let _ = fs::remove_dir_all(&dir);

        assert!(error.to_string().contains("DISCORD_TOKEN"));
    }

    #[test]
    fn an_empty_dump_is_replayed_through_a_client_made_on_the_runtime() {
        let dir = test_dir("empty");

        let outcome = replay(&dir, 100, Some("token".to_owned()));
        let _ = fs::remove_dir_all(&dir);

        assert!(outcome.unwrap());
    }
}
