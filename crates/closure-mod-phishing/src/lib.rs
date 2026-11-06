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
        let kind = match self.checker.check(question).await {
            Ok(LinkVerdict::Safe) => None,
            Ok(LinkVerdict::Unsafe(kind)) => Some(kind),
            Err(error) => {
                tracing::warn!(%error, "the link check did not answer; the message is left alone");
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

/// The links of a message, as the one text the service is asked about.
fn question(content: &str) -> Option<String> {
    let links: Vec<String> = text::links(content).into_iter().map(|l| l.url).collect();
    if links.is_empty() {
        return None;
    }
    let joined = links.join(" ");
    Some(joined.chars().take(MAX_QUESTION).collect())
}

async fn on_message(
    state: Arc<State>,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = &event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot {
        return Ok(Flow::Continue);
    }
    let Some(question) = question(&message.content) else {
        return Ok(Flow::Continue);
    };
    let Some(kind) = state.threat(&question).await else {
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
            .on(
                Subscription::new(EventKind::MessageCreated),
                move |ctx, event| on_message(state.clone(), ctx, event),
            )
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
