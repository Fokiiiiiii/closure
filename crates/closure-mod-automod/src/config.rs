//! The module's configuration: the server's settings, and the rules read one at a time.
//!
//! The JSON Schema refuses a bad rule when it is saved. This reading is the safety net: a rule
//! that is still wrong is kept as [`Entry::Invalid`] with its reason, never runs, and is listed by
//! `/automod status`. The rules are an object keyed by id, and they are checked in the order of
//! their ids, since the configuration does not keep the order it was written in.

use std::collections::BTreeMap;
use std::time::Duration;

use closure_sdk::duration;
use closure_sdk::punish::Method;
use closure_sdk::template::Template;
use closure_sdk::{ChannelId, RoleId};
use regex::{Regex, RegexBuilder};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use twilight_model::id::Id;

/// How many rules are read at all; the schema allows the same.
pub const MAX_RULES: usize = 100;
const MAX_TRIGGERS: usize = 20;
const MAX_ACTIONS: usize = 12;
const MAX_PATTERNS: usize = 20;
const MAX_PATTERN_CHARS: usize = 200;
/// A pattern whose compiled form is bigger than this is refused: the module keeps every rule's
/// regexes in memory.
const REGEX_SIZE_LIMIT: usize = 256 * 1024;
const REGEX_DFA_LIMIT: usize = 1024 * 1024;
/// Discord's longest timeout.
const MAX_TIMEOUT: Duration = Duration::from_secs(28 * 86_400);
/// The longest spam window: recent actions are kept for an hour.
const MAX_WINDOW: Duration = Duration::from_secs(3_600);
const MAX_COOLDOWN: Duration = Duration::from_secs(30 * 86_400);
/// How far back a purge looks; the history keeps seven days by default.
const MAX_PURGE: Duration = Duration::from_secs(7 * 86_400);
const MAX_AUTO_DELETE: Duration = Duration::from_secs(7 * 86_400);
const MAX_DELETE_DAYS: u32 = 7;
const MAX_AMOUNT: u32 = 10_000;

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
    /// The rules as written, keyed by id. Each one is read on its own, see [`parse_rules`].
    pub rules: BTreeMap<String, Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dry_run: false,
            warn_expire_days: 30,
            dm_on_action: true,
            timeout_method: Method::Discord,
            timeout_role: None,
            rules: BTreeMap::new(),
        }
    }
}

/// One rule of the configuration, read and checked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// The key of the rule in the configuration; set when it is read.
    #[serde(skip)]
    pub id: String,
    /// The name shown in logs and reasons. Defaults to the id.
    #[serde(default)]
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Any one of them tripping trips the rule.
    pub triggers: Vec<Trigger>,
    /// Run in order.
    pub actions: Vec<Action>,
    #[serde(default)]
    pub scope: Scope,
    /// After the rule fired for a member, it is skipped for that member for this long.
    #[serde(default, deserialize_with = "readable_option")]
    pub cooldown: Option<Duration>,
    /// Whether the rules after this one still run when this one trips.
    #[serde(default)]
    pub continue_after_match: bool,
}

/// Where a rule applies. Empty lists leave a dimension open.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Scope {
    #[serde(deserialize_with = "snowflake_list")]
    pub channels: Vec<ChannelId>,
    #[serde(deserialize_with = "snowflake_list")]
    pub exclude_channels: Vec<ChannelId>,
    #[serde(deserialize_with = "snowflake_list")]
    pub roles: Vec<RoleId>,
    #[serde(deserialize_with = "snowflake_list")]
    pub exclude_roles: Vec<RoleId>,
    pub include_bots: bool,
    pub exempt_moderators: bool,
    pub member_age: MemberAge,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            exclude_channels: Vec::new(),
            roles: Vec::new(),
            exclude_roles: Vec::new(),
            include_bots: false,
            exempt_moderators: true,
            member_age: MemberAge::default(),
        }
    }
}

/// How many days ago the member joined: at least `min_days` and at most `max_days`, when given.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberAge {
    pub min_days: Option<u32>,
    pub max_days: Option<u32>,
}

/// One trigger of a rule. The tag is `type`; a message is read once per trigger.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    AnyMessage(Empty),
    Words(Words),
    Regex(Patterns),
    Links(Links),
    Invites(Invites),
    Attachments(Attachments),
    Caps(Caps),
    Repeat(Repeat),
    Stickers(Stickers),
    Spam(Spam),
}

/// The options of a trigger or an action that has none; an unknown key is refused.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// Words, found in the message or the username.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    reason = "the fields are the options of the trigger, named as the configuration names them"
)]
pub struct Words {
    pub words: Vec<String>,
    #[serde(default)]
    pub case_sensitive: bool,
    /// A word has to stand on its own. Japanese has no spaces, so it rarely does: use `false`.
    #[serde(default = "yes")]
    pub whole_words: bool,
    /// Folds width and case, see `closure_sdk::text::normalize`.
    #[serde(default)]
    pub normalize: bool,
    /// Drops `* _ ~ | ` and backslashes before matching, so `sp*am` is `spam`.
    #[serde(default)]
    pub strip_markdown: bool,
    #[serde(default = "message_only")]
    pub targets: Vec<Target>,
}

/// Regular expressions; a message trips when one of them matches.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    reason = "the fields are the options of the trigger, named as the configuration names them"
)]
pub struct Patterns {
    pub patterns: Vec<String>,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub normalize: bool,
    #[serde(default)]
    pub strip_markdown: bool,
    #[serde(default = "message_only")]
    pub targets: Vec<Target>,
    /// Set by [`Trigger::prepare`]; a pattern that does not compile makes the rule invalid.
    #[serde(skip)]
    pub compiled: Vec<Regex>,
}

/// Links in the message.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_field_names,
    reason = "the fields are the options of the trigger, named as the configuration names them"
)]
pub struct Links {
    #[serde(default)]
    pub mode: LinkMode,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default = "yes")]
    pub include_subdomains: bool,
    /// Only `http://`, `https://` and `www.` addresses count. Off: a bare `example.org` counts too.
    #[serde(default = "yes")]
    pub only_real_links: bool,
    /// When given, only a link whose URL matches one of these is looked at.
    #[serde(default)]
    pub include_regex: Vec<String>,
    /// A link whose URL matches one of these is never looked at.
    #[serde(default)]
    pub exclude_regex: Vec<String>,
    #[serde(skip)]
    pub include: Vec<Regex>,
    #[serde(skip)]
    pub exclude: Vec<Regex>,
}

/// Invites to other servers in the message.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invites {
    /// Invites with these codes are fine; any other invite trips the rule.
    #[serde(default)]
    pub allow_codes: Vec<String>,
    /// Invites with these codes always trip, even when they are allowed.
    #[serde(default)]
    pub block_codes: Vec<String>,
}

/// Files attached to the message.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachments {
    /// Trips when the message has at least this many files.
    pub min_count: Option<u32>,
    /// Trips when the message has more than this many files.
    pub max_count: Option<u32>,
    #[serde(default)]
    pub blocked_extensions: Vec<String>,
    /// When given, a file with any other extension trips the rule.
    #[serde(default)]
    pub allowed_extensions: Vec<String>,
    /// `image/*` or an exact type.
    #[serde(default)]
    pub blocked_mime_types: Vec<String>,
    /// When given, a file of any other type (or of an unknown type) trips the rule.
    #[serde(default)]
    pub allowed_mime_types: Vec<String>,
}

/// Capital letters in the message.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caps {
    pub max_ratio: f64,
    #[serde(default = "ten")]
    pub min_letters: u32,
}

/// One character repeated in a row.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repeat {
    pub max_run: u32,
}

/// Stickers in the message.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stickers {
    pub max: u32,
}

/// A count of something across the messages of a member within a window.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spam {
    pub measure: Measure,
    /// Trips when the measure, summed over the window, reaches this.
    pub amount: u32,
    /// The window. Without one, the single message is measured.
    #[serde(default, deserialize_with = "readable_option")]
    pub within: Option<Duration>,
    /// Counts each channel on its own.
    #[serde(default)]
    pub per_channel: bool,
    /// The window has to span this many channels.
    pub distinct_channels: Option<u32>,
    /// A text that was sent before in the window counts once.
    #[serde(default)]
    pub ignore_identical_messages: bool,
    /// Every counted message has to be the same text.
    #[serde(default)]
    pub group_identical_messages: bool,
}

/// What a spam trigger counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Measure {
    Messages,
    Mentions,
    Links,
    Attachments,
    Emojis,
    Lines,
    Characters,
    Stickers,
    /// The most often repeated text in the window.
    Duplicates,
}

/// Where a word trigger looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    Message,
    Username,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkMode {
    /// Any link at all.
    #[default]
    Any,
    /// Any link whose domain is not in the list.
    Allowlist,
    /// Any link whose domain is in the list.
    Blocklist,
}

/// What a rule does once it hits, in the order given.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// The messages of the hit: a bulk delete within one channel, single deletes otherwise.
    Delete(Empty),
    /// Deletes what the member sent within this long, from the message history.
    Purge {
        #[serde(deserialize_with = "readable")]
        within: Duration,
    },
    Warn {
        #[serde(default)]
        reason: Option<String>,
    },
    Timeout {
        #[serde(deserialize_with = "readable")]
        duration: Duration,
        #[serde(default)]
        reason: Option<String>,
    },
    Kick {
        #[serde(default)]
        reason: Option<String>,
    },
    Ban {
        #[serde(default, deserialize_with = "readable_option")]
        duration: Option<Duration>,
        #[serde(default)]
        delete_days: u32,
        #[serde(default)]
        reason: Option<String>,
    },
    /// A message in the channel of the hit. It mentions nobody.
    Reply {
        text: String,
        #[serde(default, deserialize_with = "readable_option")]
        auto_delete: Option<Duration>,
    },
    /// A message in the channel given. It mentions nobody.
    Alert {
        #[serde(deserialize_with = "snowflake_field")]
        channel: ChannelId,
        #[serde(default)]
        text: Option<String>,
    },
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
pub fn parse_rules(raw: &BTreeMap<String, Value>) -> Vec<Entry> {
    raw.iter()
        .enumerate()
        .map(|(index, (id, value))| {
            if index >= MAX_RULES {
                return Entry::Invalid {
                    label: id.clone(),
                    why: Problem::Key("problem-too-many"),
                };
            }
            match read(id, value.clone()) {
                Ok(rule) => Entry::Valid(rule),
                Err(why) => Entry::Invalid {
                    label: id.clone(),
                    why,
                },
            }
        })
        .collect()
}

fn read(id: &str, value: Value) -> Result<Rule, Problem> {
    let mut rule: Rule =
        serde_json::from_value(value).map_err(|error| Problem::Shape(error.to_string()))?;
    if !valid_id(id) {
        return Err(Problem::Key("problem-id"));
    }
    id.clone_into(&mut rule.id);
    if rule.name.is_empty() {
        id.clone_into(&mut rule.name);
    }
    if rule.name.trim().is_empty() || rule.name.chars().count() > 64 {
        return Err(Problem::Key("problem-name"));
    }
    if rule.triggers.is_empty() || rule.triggers.len() > MAX_TRIGGERS {
        return Err(Problem::Key("problem-triggers"));
    }
    for trigger in &mut rule.triggers {
        trigger.prepare()?;
    }
    if rule.actions.is_empty() || rule.actions.len() > MAX_ACTIONS {
        return Err(Problem::Key("problem-actions"));
    }
    for action in &rule.actions {
        action.validate()?;
    }
    if rule
        .cooldown
        .is_some_and(|cooldown| cooldown.is_zero() || cooldown > MAX_COOLDOWN)
    {
        return Err(Problem::Key("problem-cooldown"));
    }
    rule.scope.validate()?;
    Ok(rule)
}

/// Rule ids are the names of the store's entries too, so they are kept to plain characters.
fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn yes() -> bool {
    true
}

fn ten() -> u32 {
    10
}

fn message_only() -> Vec<Target> {
    vec![Target::Message]
}

impl Scope {
    fn validate(&self) -> Result<(), Problem> {
        let age = self.member_age;
        let ordered = match (age.min_days, age.max_days) {
            (Some(min), Some(max)) => min <= max,
            _ => true,
        };
        let in_range = [age.min_days, age.max_days]
            .into_iter()
            .flatten()
            .all(|days| days <= 36_500);
        if ordered && in_range {
            Ok(())
        } else {
            Err(Problem::Key("problem-member-age"))
        }
    }
}

impl Trigger {
    /// Checks the trigger's ranges and compiles its patterns. Called once, when the rule is read.
    fn prepare(&mut self) -> Result<(), Problem> {
        match self {
            Self::AnyMessage(_) => Ok(()),
            Self::Words(words) => words.check(),
            Self::Regex(patterns) => patterns.compile(),
            Self::Links(links) => links.compile(),
            Self::Invites(invites) => invites.check(),
            Self::Attachments(files) => files.check(),
            Self::Caps(caps) => caps.check(),
            Self::Repeat(repeat) => repeat.check(),
            Self::Stickers(stickers) => stickers.check(),
            Self::Spam(spam) => spam.check(),
        }
    }
}

impl Words {
    fn check(&self) -> Result<(), Problem> {
        let ok = (1..=500).contains(&self.words.len())
            && self.words.iter().all(|word| {
                let length = word.trim().chars().count();
                (1..=100).contains(&length)
            })
            && !self.targets.is_empty();
        if ok {
            Ok(())
        } else {
            Err(Problem::Key("problem-words"))
        }
    }
}

impl Patterns {
    fn compile(&mut self) -> Result<(), Problem> {
        if self.patterns.is_empty() || self.patterns.len() > MAX_PATTERNS {
            return Err(Problem::Key("problem-patterns"));
        }
        self.compiled = compile_all(&self.patterns, !self.case_sensitive)?;
        Ok(())
    }
}

impl Links {
    fn compile(&mut self) -> Result<(), Problem> {
        let ok = self.domains.len() <= 200
            && self.domains.iter().all(|domain| !domain.trim().is_empty())
            && self.include_regex.len() <= 50
            && self.exclude_regex.len() <= 50;
        if !ok {
            return Err(Problem::Key("problem-domains"));
        }
        // Hosts and URLs are read without regard to case.
        self.include = compile_all(&self.include_regex, true)?;
        self.exclude = compile_all(&self.exclude_regex, true)?;
        Ok(())
    }
}

impl Invites {
    fn check(&self) -> Result<(), Problem> {
        let ok = self.allow_codes.len() <= 100
            && self.block_codes.len() <= 100
            && self
                .allow_codes
                .iter()
                .chain(&self.block_codes)
                .all(|code| !code.trim().is_empty());
        if ok {
            Ok(())
        } else {
            Err(Problem::Key("problem-invites"))
        }
    }
}

impl Attachments {
    fn check(&self) -> Result<(), Problem> {
        let counts = self.min_count.is_none_or(|min| (1..=10).contains(&min))
            && self.max_count.is_none_or(|max| max <= 10);
        let lists = [
            &self.blocked_extensions,
            &self.allowed_extensions,
            &self.blocked_mime_types,
            &self.allowed_mime_types,
        ];
        let lists_ok = lists.iter().all(|list| list.len() <= 50);
        let something = self.min_count.is_some()
            || self.max_count.is_some()
            || lists.iter().any(|list| !list.is_empty());
        if counts && lists_ok && something {
            Ok(())
        } else {
            Err(Problem::Key("problem-attachments"))
        }
    }
}

impl Caps {
    fn check(&self) -> Result<(), Problem> {
        let ok = (0.0..=1.0).contains(&self.max_ratio) && (1..=2000).contains(&self.min_letters);
        if ok {
            Ok(())
        } else {
            Err(Problem::Key("problem-caps"))
        }
    }
}

impl Repeat {
    fn check(&self) -> Result<(), Problem> {
        if (2..=2000).contains(&self.max_run) {
            Ok(())
        } else {
            Err(Problem::Key("problem-repeat"))
        }
    }
}

impl Stickers {
    fn check(&self) -> Result<(), Problem> {
        if self.max <= 10 {
            Ok(())
        } else {
            Err(Problem::Key("problem-stickers"))
        }
    }
}

impl Spam {
    fn check(&self) -> Result<(), Problem> {
        if !(1..=MAX_AMOUNT).contains(&self.amount) {
            return Err(Problem::Key("problem-amount"));
        }
        // The measures that compare with other messages need a window to be about.
        let windowed = self.per_channel
            || self.distinct_channels.is_some()
            || self.measure == Measure::Duplicates;
        match self.within {
            Some(window) if window.is_zero() || window > MAX_WINDOW => {
                return Err(Problem::Key("problem-window"));
            }
            None if windowed => return Err(Problem::Key("problem-window")),
            _ => {}
        }
        if self
            .distinct_channels
            .is_some_and(|distinct| !(2..=50).contains(&distinct))
        {
            return Err(Problem::Key("problem-distinct"));
        }
        Ok(())
    }
}

impl Action {
    fn validate(&self) -> Result<(), Problem> {
        match self {
            Self::Delete(_) | Self::Kick { reason: None } | Self::Warn { reason: None } => Ok(()),
            Self::Kick { reason } | Self::Warn { reason } => reason_ok(reason.as_deref()),
            Self::Purge { within } => {
                if within.is_zero() || *within > MAX_PURGE {
                    Err(Problem::Key("problem-purge"))
                } else {
                    Ok(())
                }
            }
            Self::Timeout { duration, reason } => {
                if duration.is_zero() || *duration > MAX_TIMEOUT {
                    return Err(Problem::Key("problem-timeout"));
                }
                reason_ok(reason.as_deref())
            }
            Self::Ban {
                duration,
                delete_days,
                reason,
            } => {
                if duration.is_some_and(|duration| duration.is_zero()) {
                    return Err(Problem::Key("problem-duration"));
                }
                if *delete_days > MAX_DELETE_DAYS {
                    return Err(Problem::Key("problem-delete-days"));
                }
                reason_ok(reason.as_deref())
            }
            Self::Reply { text, auto_delete } => {
                let length = text.chars().count();
                if !(1..=2000).contains(&length) {
                    return Err(Problem::Key("problem-text"));
                }
                if auto_delete.is_some_and(|after| after.is_zero() || after > MAX_AUTO_DELETE) {
                    return Err(Problem::Key("problem-auto-delete"));
                }
                reason_ok(Some(text))
            }
            Self::Alert { text, .. } => reason_ok(text.as_deref()),
        }
    }
}

fn reason_ok(text: Option<&str>) -> Result<(), Problem> {
    match text {
        Some(text) if Template::parse(text).is_err() => Err(Problem::Key("problem-template")),
        _ => Ok(()),
    }
}

/// Compiles the patterns with the limits that keep a hostile one from using the memory.
fn compile_all(patterns: &[String], case_insensitive: bool) -> Result<Vec<Regex>, Problem> {
    patterns
        .iter()
        .map(|pattern| {
            let length = pattern.chars().count();
            if !(1..=MAX_PATTERN_CHARS).contains(&length) {
                return Err(Problem::Key("problem-pattern"));
            }
            RegexBuilder::new(pattern)
                .case_insensitive(case_insensitive)
                .size_limit(REGEX_SIZE_LIMIT)
                .dfa_size_limit(REGEX_DFA_LIMIT)
                .build()
                .map_err(|_| Problem::Key("problem-regex"))
        })
        .collect()
}

/// The duration a field is written with, such as `10m` or `1h 30m`.
fn readable<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    duration::parse(&text).map_err(|error| D::Error::custom(format!("{text:?}: {error}")))
}

fn readable_option<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Duration>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|text| {
            duration::parse(&text).map_err(|error| D::Error::custom(format!("{text:?}: {error}")))
        })
        .transpose()
}

/// A Discord id as the configuration writes it: digits only, and not zero.
fn snowflake<T>(text: &str) -> Option<Id<T>> {
    if text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Id::new_checked(text.parse().ok()?)
}

fn snowflake_field<'de, D: Deserializer<'de>, T>(deserializer: D) -> Result<Id<T>, D::Error> {
    let text = String::deserialize(deserializer)?;
    snowflake(&text).ok_or_else(|| D::Error::custom(format!("{text:?} is not a Discord id")))
}

fn snowflake_list<'de, D: Deserializer<'de>, T>(deserializer: D) -> Result<Vec<Id<T>>, D::Error> {
    let texts = Vec::<String>::deserialize(deserializer)?;
    texts
        .iter()
        .map(|text| {
            snowflake(text).ok_or_else(|| D::Error::custom(format!("{text:?} is not a Discord id")))
        })
        .collect()
}

/// The role a timeout gives, when the configuration names one that is a valid id.
pub fn role_of(text: Option<&str>) -> Option<RoleId> {
    text.and_then(snowflake)
}

/// The key of a trigger's name, as the status and a dry run show it.
pub fn trigger_key(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::AnyMessage(_) => "trigger-any-message",
        Trigger::Words(_) => "trigger-words",
        Trigger::Regex(_) => "trigger-regex",
        Trigger::Links(_) => "trigger-links",
        Trigger::Invites(_) => "trigger-invites",
        Trigger::Attachments(_) => "trigger-attachments",
        Trigger::Caps(_) => "trigger-caps",
        Trigger::Repeat(_) => "trigger-repeat",
        Trigger::Stickers(_) => "trigger-stickers",
        Trigger::Spam(_) => "trigger-spam",
    }
}

/// The key of an action's name, as the status and a dry run show it.
pub fn action_key(action: &Action) -> &'static str {
    match action {
        Action::Delete(_) => "action-delete",
        Action::Purge { .. } => "action-purge",
        Action::Warn { .. } => "action-warn",
        Action::Timeout { .. } => "action-timeout",
        Action::Kick { .. } => "action-kick",
        Action::Ban { .. } => "action-ban",
        Action::Reply { .. } => "action-reply",
        Action::Alert { .. } => "action-alert",
    }
}
