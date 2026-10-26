//! The module's configuration: the server's settings, and the rules read one at a time.
//!
//! The JSON Schema refuses a bad rule when it is saved. This reading is the safety net: a rule
//! that is still wrong is kept as [`Entry::Invalid`] with its reason, never runs, and is listed by
//! `/automod status`. Duplicate ids are the same kind of mistake: the first valid rule keeps the id.

use std::collections::HashSet;
use std::time::Duration;

use closure_sdk::duration;
use closure_sdk::punish::Method;
use closure_sdk::{ChannelId, RoleId};
use serde::Deserialize;
use serde_json::Value;
use twilight_model::id::Id;

/// How many rules are read at all; the schema allows the same.
pub const MAX_RULES: usize = 50;
const MAX_ACTIONS: usize = 8;
/// Discord's longest timeout.
const MAX_TIMEOUT: Duration = Duration::from_secs(28 * 86_400);
const MAX_DELETE_DAYS: u32 = 7;

/// The module's section of a guild's configuration.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Acts on nothing; reports what would have run to the log channel instead.
    pub dry_run: bool,
    /// After how many days a warning of a rule expires.
    pub warn_expire_days: u32,
    /// Whether the member gets a direct message about a punishment.
    pub dm_on_action: bool,
    /// How a timeout is given: Discord's timeout, or a role that a timer takes off again.
    pub timeout_method: Method,
    /// The id of the role a timeout gives, when `timeout_method` is `role`.
    pub timeout_role: Option<String>,
    /// The rules as written. Each one is read on its own, see [`parse_rules`].
    pub rules: Vec<Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dry_run: false,
            warn_expire_days: 30,
            dm_on_action: true,
            timeout_method: Method::Discord,
            timeout_role: None,
            rules: Vec::new(),
        }
    }
}

/// One rule of the configuration, read and checked.
#[derive(Debug)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub check: Check,
    pub scope: Scope,
    pub actions: Vec<Action>,
}

/// Where a rule applies. Empty lists leave a dimension open.
#[derive(Debug)]
pub struct Scope {
    pub channels: Vec<ChannelId>,
    pub exclude_channels: Vec<ChannelId>,
    pub roles: Vec<RoleId>,
    pub exclude_roles: Vec<RoleId>,
    pub include_bots: bool,
    pub exempt_moderators: bool,
}

/// What a rule looks for in a message.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Check {
    Words {
        words: Vec<String>,
        #[serde(default)]
        r#match: Matching,
    },
    Links {
        mode: LinkMode,
        #[serde(default)]
        domains: Vec<String>,
    },
    Invites {
        #[serde(default)]
        allow: Vec<String>,
    },
    Mentions {
        max: u32,
    },
    Caps {
        max_ratio: f64,
        #[serde(default = "ten")]
        min_letters: u32,
    },
    Repeat {
        max_run: u32,
    },
    Emoji {
        max: u32,
    },
    Attachments {
        max: Option<u32>,
        #[serde(default)]
        blocked_extensions: Vec<String>,
    },
    /// Counted on new messages only, per author and per rule; the window is in whole minutes.
    Flood {
        messages: u32,
        per_minutes: u32,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Matching {
    /// The word occurs anywhere, also inside a longer word.
    Contains,
    /// The word stands on its own. Japanese has no spaces, so prefer `contains` for it.
    #[default]
    Whole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkMode {
    /// Any link at all.
    Any,
    /// Any link whose domain is not in the list.
    Allowlist,
    /// Any link whose domain is in the list.
    Blocklist,
}

/// What a rule does once it hits, in the order given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Delete,
    Warn,
    Timeout(Duration),
    Kick,
    Ban {
        duration: Option<Duration>,
        delete_days: u32,
    },
    Notify(ChannelId),
}

/// Why a rule is not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The rule does not have the shape the schema describes; the parser says where.
    Shape(String),
    /// One of the rule's own values is out of range; the key names the message.
    Key(&'static str),
}

/// A rule as the configuration gave it: a valid rule, or the reason it cannot run.
#[derive(Debug)]
pub enum Entry {
    Valid(Rule),
    Invalid { label: String, why: Problem },
}

/// Reads the rules one by one. A rule that is not valid is kept as [`Entry::Invalid`] rather than
/// refusing the whole list.
pub fn parse_rules(raw: &[Value]) -> Vec<Entry> {
    let mut claimed = HashSet::new();
    raw.iter()
        .enumerate()
        .map(|(index, value)| {
            let label = value
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| format!("#{}", index + 1), str::to_owned);
            if index >= MAX_RULES {
                return Entry::Invalid {
                    label,
                    why: Problem::Key("problem-too-many"),
                };
            }
            match read(value.clone()) {
                Ok(rule) if claimed.contains(&rule.id) => Entry::Invalid {
                    label: rule.id,
                    why: Problem::Key("problem-duplicate"),
                },
                Ok(rule) => {
                    claimed.insert(rule.id.clone());
                    Entry::Valid(rule)
                }
                Err(why) => Entry::Invalid { label, why },
            }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    id: String,
    name: Option<String>,
    #[serde(default = "yes")]
    enabled: bool,
    check: Check,
    #[serde(default)]
    scope: ScopeSpec,
    actions: Vec<ActionSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ScopeSpec {
    channels: Vec<String>,
    exclude_channels: Vec<String>,
    roles: Vec<String>,
    exclude_roles: Vec<String>,
    include_bots: bool,
    exempt_moderators: bool,
}

impl Default for ScopeSpec {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            exclude_channels: Vec::new(),
            roles: Vec::new(),
            exclude_roles: Vec::new(),
            include_bots: false,
            exempt_moderators: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case", deny_unknown_fields)]
enum ActionSpec {
    Delete,
    Warn,
    Timeout {
        duration: String,
    },
    Kick,
    Ban {
        duration: Option<String>,
        #[serde(default)]
        delete_days: u32,
    },
    Notify {
        channel: String,
    },
}

fn yes() -> bool {
    true
}

fn ten() -> u32 {
    10
}

fn read(value: Value) -> Result<Rule, Problem> {
    let spec: RuleSpec =
        serde_json::from_value(value).map_err(|error| Problem::Shape(error.to_string()))?;
    if !valid_id(&spec.id) {
        return Err(Problem::Key("problem-id"));
    }
    let name = spec.name.unwrap_or_else(|| spec.id.clone());
    if name.trim().is_empty() || name.chars().count() > 64 {
        return Err(Problem::Key("problem-name"));
    }
    spec.check.validate()?;
    if spec.actions.is_empty() || spec.actions.len() > MAX_ACTIONS {
        return Err(Problem::Key("problem-actions"));
    }
    let actions = spec
        .actions
        .into_iter()
        .map(action)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Rule {
        id: spec.id,
        name,
        enabled: spec.enabled,
        check: spec.check,
        scope: scope(&spec.scope)?,
        actions,
    })
}

/// Rule ids are the names of the store's counters too, so they are kept to plain characters.
fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn scope(spec: &ScopeSpec) -> Result<Scope, Problem> {
    Ok(Scope {
        channels: ids(&spec.channels)?,
        exclude_channels: ids(&spec.exclude_channels)?,
        roles: ids(&spec.roles)?,
        exclude_roles: ids(&spec.exclude_roles)?,
        include_bots: spec.include_bots,
        exempt_moderators: spec.exempt_moderators,
    })
}

fn action(spec: ActionSpec) -> Result<Action, Problem> {
    Ok(match spec {
        ActionSpec::Delete => Action::Delete,
        ActionSpec::Warn => Action::Warn,
        ActionSpec::Timeout { duration } => {
            let duration = readable(&duration)?;
            if duration > MAX_TIMEOUT {
                return Err(Problem::Key("problem-timeout"));
            }
            Action::Timeout(duration)
        }
        ActionSpec::Kick => Action::Kick,
        ActionSpec::Ban {
            duration,
            delete_days,
        } => {
            if delete_days > MAX_DELETE_DAYS {
                return Err(Problem::Key("problem-delete-days"));
            }
            Action::Ban {
                duration: duration.as_deref().map(readable).transpose()?,
                delete_days,
            }
        }
        ActionSpec::Notify { channel } => {
            Action::Notify(snowflake(&channel).ok_or(Problem::Key("problem-id-value"))?)
        }
    })
}

fn readable(text: &str) -> Result<Duration, Problem> {
    duration::parse(text).map_err(|_| Problem::Key("problem-duration"))
}

/// A Discord id as the configuration writes it: digits only, and not zero.
fn snowflake<T>(text: &str) -> Option<Id<T>> {
    if text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Id::new_checked(text.parse().ok()?)
}

fn ids<T>(texts: &[String]) -> Result<Vec<Id<T>>, Problem> {
    texts
        .iter()
        .map(|text| snowflake(text).ok_or(Problem::Key("problem-id-value")))
        .collect()
}

/// The role a timeout gives, when the configuration names one that is a valid id.
pub fn role_of(text: Option<&str>) -> Option<RoleId> {
    text.and_then(snowflake)
}

impl Check {
    /// The ranges the schema states too; a rule that breaks one is invalid.
    fn validate(&self) -> Result<(), Problem> {
        let ok = match self {
            Self::Words { words, .. } => {
                (1..=500).contains(&words.len())
                    && words
                        .iter()
                        .all(|word| (1..=100).contains(&word.trim().chars().count()))
            }
            Self::Links { domains, .. } => {
                domains.len() <= 200 && domains.iter().all(|domain| !domain.trim().is_empty())
            }
            Self::Invites { allow } => {
                allow.len() <= 100 && allow.iter().all(|code| !code.trim().is_empty())
            }
            Self::Mentions { max } => (1..=50).contains(max),
            Self::Caps {
                max_ratio,
                min_letters,
            } => (0.0..=1.0).contains(max_ratio) && (1..=2000).contains(min_letters),
            Self::Repeat { max_run } => (2..=2000).contains(max_run),
            Self::Emoji { max } => (1..=200).contains(max),
            Self::Attachments {
                max,
                blocked_extensions,
            } => {
                max.is_none_or(|max| max <= 10) && (max.is_some() || !blocked_extensions.is_empty())
            }
            Self::Flood {
                messages,
                per_minutes,
            } => (2..=100).contains(messages) && (1..=60).contains(per_minutes),
        };
        if ok {
            return Ok(());
        }
        Err(Problem::Key(match self {
            Self::Words { .. } => "problem-words",
            Self::Links { .. } => "problem-domains",
            Self::Invites { .. } => "problem-invites",
            Self::Mentions { .. } => "problem-mentions",
            Self::Caps { .. } => "problem-caps",
            Self::Repeat { .. } => "problem-repeat",
            Self::Emoji { .. } => "problem-emoji",
            Self::Attachments { .. } => "problem-attachments",
            Self::Flood { .. } => "problem-flood",
        }))
    }
}
