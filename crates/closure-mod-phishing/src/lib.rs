//! Removes the messages whose links are flagged as phishing or scams by an external service.
//!
//! The host gives the module the service ([`LinkChecker`]); the module only asks it about the
//! links of a message, remembers the answer for a few minutes, and acts on a flagged message. When
//! the service cannot be reached the message is left alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use closure_core::action::Reason;
use closure_core::event::{Event, EventKind};
use closure_core::links::{LinkChecker, LinkVerdict};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, CoreError, Intents, Locale, ModuleBuilder, ModuleError, SdkModule, Subscription, text,
};
use serde::Deserialize;
use serde_json::json;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// How long an answer of the service is remembered.
const REMEMBER: Duration = Duration::from_secs(600);
/// How many answers are remembered; past this the oldest half goes.
const MAX_REMEMBERED: usize = 2000;
/// The most characters of links sent in one question.
const MAX_QUESTION: usize = 1500;
/// The most questions one message is asked about: the service is not asked about a wall of links
/// without end.
const MAX_QUESTIONS: usize = 4;
/// After the service fails to answer, it is not asked for this long: messages are left alone at
/// once instead of each waiting its turn on a service that is down.
const BACK_OFF: Duration = Duration::from_secs(15);

/// What happens to the member who posted a flagged link.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Punishment {
    #[default]
    Ban,
    Kick,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    action: Punishment,
}

/// A banned member's messages of this long ago are removed too: a phishing account tends to post
/// the same link in several channels.
const BAN_CLEANUP_SECONDS: u32 = 3600;

struct State {
    checker: Arc<dyn LinkChecker>,
    seen: Mutex<HashMap<String, (Instant, Option<String>)>>,
    /// Until when the service is left alone, after it failed to answer.
    down_until: Mutex<Option<Instant>>,
}

impl State {
    /// The kind of threat the service named, if it flagged the links; `None` for safe links and
    /// for a service that did not answer.
    async fn threat(&self, question: &str) -> Option<String> {
        let now = Instant::now();
        if let Some((at, kind)) = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(question)
            && now.duration_since(*at) < REMEMBER
        {
            return kind.clone();
        }
        if self
            .down_until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|until| now < until)
        {
            return None;
        }
        let kind = match self.checker.check(question).await {
            Ok(LinkVerdict::Safe) => None,
            Ok(LinkVerdict::Unsafe(kind)) => Some(kind),
            Err(error) => {
                tracing::warn!(%error, "the link check did not answer; the message is left alone");
                *self
                    .down_until
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(now + BACK_OFF);
                return None;
            }
        };
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if seen.len() >= MAX_REMEMBERED {
            let mut times: Vec<Instant> = seen.values().map(|(at, _)| *at).collect();
            times.sort_unstable();
            let cut = times[times.len() / 2];
            seen.retain(|_, (at, _)| *at > cut);
        }
        seen.insert(question.to_owned(), (now, kind.clone()));
        kind
    }
}

/// The links of a message, as the texts the service is asked about: as many links as fit in one
/// question, never cut in the middle of a link.
fn questions(content: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for link in text::links(content).into_iter().map(|l| l.url) {
        let link: String = link.chars().take(MAX_QUESTION).collect();
        match out.last_mut() {
            Some(last) if last.chars().count() + 1 + link.chars().count() <= MAX_QUESTION => {
                last.push(' ');
                last.push_str(&link);
            }
            _ => {
                if out.len() == MAX_QUESTIONS {
                    break;
                }
                out.push(link);
            }
        }
    }
    out
}

async fn on_message(
    state: Arc<State>,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    // An edit is checked as well: a link can be put in a message that was clean.
    let (Event::MessageCreated(message) | Event::MessageUpdated(message)) = &event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot {
        return Ok(Flow::Continue);
    }
    let mut flagged = None;
    for question in questions(&message.content) {
        if let Some(kind) = state.threat(&question).await {
            flagged = Some(kind);
            break;
        }
    }
    let Some(kind) = flagged else {
        return Ok(Flow::Continue);
    };
    let config: Config = ctx.parse_config()?;
    let reason = Reason::new(ctx.t_in(
        ctx.locale,
        "phishing-reason",
        &Args::new().with("kind", kind),
    ))?;
    if let Err(error) = ctx
        .api
        .delete_message(message.channel_id, message.id, reason.clone())
        .await
    {
        tracing::debug!(?error, guild = %ctx.guild_id, "could not remove a flagged message");
    }
    let result = match config.action {
        Punishment::Ban => {
            ctx.api
                .ban(ctx.guild_id, message.author.id, BAN_CLEANUP_SECONDS, reason)
                .await
        }
        Punishment::Kick => ctx.api.kick(ctx.guild_id, message.author.id, reason).await,
    };
    if let Err(error) = result {
        tracing::warn!(?error, guild = %ctx.guild_id, "could not remove the author of a flagged link");
    }
    Ok(Flow::Continue)
}

pub fn builder(checker: Arc<dyn LinkChecker>) -> Result<ModuleBuilder, CoreError> {
    let state = Arc::new(State {
        checker,
        seen: Mutex::new(HashMap::new()),
        down_until: Mutex::new(None),
    });
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {
                "type": "string",
                "enum": ["ban", "kick"],
                "description": "config-phishing-action",
                "x-ui": { "level": "basic" }
            }
        }
    });
    Ok(
        ModuleBuilder::new("phishing", "Phishing", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::ManageMessages)
            .capability(closure_sdk::Capability::BanMembers)
            .capability(closure_sdk::Capability::KickMembers)
            .intents(Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT)
            .config(schema, json!({"action": "ban"}))
            .on(Subscription::new(EventKind::MessageCreated), {
                let state = state.clone();
                move |ctx, event| on_message(state.clone(), ctx, event)
            })
            .on(Subscription::new(EventKind::MessageUpdated), {
                let state = state.clone();
                move |ctx, event| on_message(state.clone(), ctx, event)
            })
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

pub fn module(checker: Arc<dyn LinkChecker>) -> Result<SdkModule, CoreError> {
    builder(checker)?.build()
}

#[cfg(test)]
mod tests;
