//! The module's configuration: the server's settings, and the rules read one at a time.
//!
//! The JSON Schema refuses a bad rule when it is saved. This reading is the safety net: a rule
//! that is still wrong is kept as [`Entry::Invalid`] with its reason, never runs, and is listed by
//! `/automod status`. The rules are an object keyed by id, and they are checked in the order of
//! their ids, since the configuration does not keep the order it was written in.

use std::collections::BTreeMap;
use std::time::Duration;

use closure_sdk::counters::{Condition, CounterDef, Counters, Decay, TriggerDef};
use closure_sdk::duration;
use closure_sdk::punish::{Method, RoleSetting};
use closure_sdk::template::Template;
use closure_sdk::{ChannelId, RoleId, Store};
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
const MAX_TIMEOUT: Duration = Duration::from_hours(28 * 24);
/// The longest spam window: recent actions are kept for an hour.
const MAX_WINDOW: Duration = Duration::from_secs(3_600);
const MAX_COOLDOWN: Duration = Duration::from_hours(30 * 24);
/// How far back a purge looks; the history keeps seven days by default.
const MAX_PURGE: Duration = Duration::from_hours(7 * 24);
const MAX_AUTO_DELETE: Duration = Duration::from_hours(7 * 24);
const MAX_DELETE_DAYS: u32 = 7;
const MAX_AMOUNT: u32 = 10_000;
/// How many counters the module keeps, and how many triggers each one has.
pub const MAX_COUNTERS: usize = 10;
const MAX_COUNTER_TRIGGERS: usize = 10;
const MAX_ROLES: usize = 25;
const MAX_CHANNELS: usize = 10;
/// Discord's longest slowmode.
const MAX_SLOWMODE: Duration = Duration::from_hours(6);
const MAX_NEW_THRESHOLD: Duration = Duration::from_hours(30 * 24);
const MAX_NICKNAME_CHARS: usize = 100;
/// The counter value Discord's counters hold at most.
const MAX_COUNTER_VALUE: i64 = 2_147_483_647;

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
    /// The id of the role a timeout gives, when `timeout_method` is `role`; left out, Closure creates one.
    pub timeout_role: Option<String>,
    /// Which roles a role timeout takes off the member: `false`, `true` (every role that can be taken
    /// off), or role ids.
    pub remove_roles_on_mute: RoleSetting,
    /// Which of the roles taken off come back when the timeout ends: the same forms, `true` by default.
    pub restore_roles_on_mute: RoleSetting,
    /// The counters, keyed by name. They are read as a whole, see [`setup`].
    pub counters: BTreeMap<String, CounterConfig>,
    /// The rules as written, keyed by id. Each one is read on its own, see [`parse_rules`].
    pub rules: BTreeMap<String, Value>,
    /// The antiraid section, read on its own by [`crate::antiraid::read`]; absent means antiraid is off.
    pub antiraid: Option<Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dry_run: false,
            warn_expire_days: 30,
            dm_on_action: true,
            timeout_method: Method::Discord,
            timeout_role: None,
            remove_roles_on_mute: RoleSetting::Flag(false),
            restore_roles_on_mute: RoleSetting::Flag(true),
            counters: BTreeMap::new(),
            rules: BTreeMap::new(),
            antiraid: None,
        }
    }
}

/// One counter as the configuration writes it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CounterConfig {
    pub per_channel: bool,
    pub per_user: bool,
    pub initial_value: i64,
    pub decay: Option<DecayConfig>,
    /// Each trigger is a condition, or a condition with the condition of its reverse.
    pub triggers: BTreeMap<String, TriggerConfig>,
}

/// The decay of a counter: it goes down by `amount` every `every`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecayConfig {
    pub amount: i64,
    #[serde(deserialize_with = "readable")]
    pub every: Duration,
}

/// A trigger of a counter: `">= 5"`, or the condition and its reverse.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum TriggerConfig {
    Short(String),
    Full(FullTrigger),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullTrigger {
    pub condition: String,
    /// When left out, the opposite of `condition`.
    pub reverse_condition: Option<String>,
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
    MemberJoin(MemberJoin),
    MemberJoinSpam(JoinSpam),
    MemberLeave(Empty),
    RoleAdded(RoleList),
    RoleRemoved(RoleList),
    Case(CaseTrigger),
    ThreadCreated(Empty),
    ThreadArchived(Empty),
    ThreadUnarchived(Empty),
    ThreadDeleted(Empty),
    Counter(CounterTrigger),
    AntiraidLevel(AntiraidTrigger),
}

/// The antiraid level changed. Empty `levels` is any level, empty `from` is any level before.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AntiraidTrigger {
    #[serde(default)]
    pub levels: Vec<String>,
    #[serde(default)]
    pub from: Vec<String>,
}

/// A member joined. With `only_new`, only an account made within `new_threshold` trips it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberJoin {
    #[serde(default)]
    pub only_new: bool,
    #[serde(default = "hour", deserialize_with = "readable")]
    pub new_threshold: Duration,
}

/// Members joined within a window, counted together; every one of them is a target.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinSpam {
    pub amount: u32,
    #[serde(deserialize_with = "readable")]
    pub within: Duration,
}

/// Role changes that add (or remove) one of these roles trip the trigger.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleList {
    #[serde(deserialize_with = "snowflake_list")]
    pub roles: Vec<RoleId>,
}

/// A case was recorded. An empty `kinds` is any kind; the bot's own cases are left out unless
/// `include_own` is set.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseTrigger {
    #[serde(default)]
    pub kinds: Vec<CaseName>,
    #[serde(default)]
    pub include_own: bool,
}

/// The kinds of case a case trigger can name: the kinds the moderation module records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseName {
    Warn,
    Note,
    Timeout,
    Untimeout,
    Kick,
    Ban,
    Unban,
    Vcmute,
    Vcunmute,
}

impl CaseName {
    /// The kind as a case records it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Note => "note",
            Self::Timeout => "timeout",
            Self::Untimeout => "untimeout",
            Self::Kick => "kick",
            Self::Ban => "ban",
            Self::Unban => "unban",
            Self::Vcmute => "vcmute",
            Self::Vcunmute => "vcunmute",
        }
    }
}

/// A trigger of a counter: it fired (or, with `reverse`, its reverse did).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CounterTrigger {
    pub counter: String,
    pub trigger: String,
    #[serde(default)]
    pub reverse: bool,
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
    AddRoles {
        #[serde(deserialize_with = "snowflake_list")]
        roles: Vec<RoleId>,
    },
    RemoveRoles {
        #[serde(deserialize_with = "snowflake_list")]
        roles: Vec<RoleId>,
    },
    /// The nickname of the member, a template.
    SetNickname {
        name: String,
    },
    /// The slowmode of the channels given, or of the channel of the event. Zero turns it off.
    Slowmode {
        #[serde(deserialize_with = "readable")]
        duration: Duration,
        #[serde(default, deserialize_with = "snowflake_list")]
        channels: Vec<ChannelId>,
    },
    AddToCounter {
        counter: String,
        amount: i64,
    },
    SetCounter {
        counter: String,
        value: i64,
    },
    /// Raises or lowers the antiraid level. A timed level goes back to the calm one after `duration`.
    SetAntiraidLevel {
        level: String,
        #[serde(default, deserialize_with = "readable_option")]
        duration: Option<Duration>,
    },
}

/// Why a rule is not run, or why the counters are not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The rule does not have the shape the schema describes; the parser says where.
    Shape(String),
    /// One of the rule's own values is out of range; the key names the message.
    Key(&'static str),
    /// The counters are not valid; the SDK says why.
    Counters(String),
}

/// A rule as the configuration gave it: a valid rule, or the reason it cannot run.
#[derive(Debug)]
pub enum Entry {
    Valid(Rule),
    Invalid { label: String, why: Problem },
}

/// The module's configuration as it is read: the rules, and the counters they may use.
#[derive(Debug)]
pub struct Setup {
    pub rules: Vec<Entry>,
    /// The counters, when they are all valid; none when they are not, see `counters_problem`.
    pub counters: Vec<CounterDef>,
    pub counters_problem: Option<Problem>,
    /// The antiraid section, when it is there and valid; see `antiraid_problem` otherwise.
    pub antiraid: Option<crate::antiraid::Antiraid>,
    pub antiraid_problem: Option<Problem>,
}

/// Reads the counters and then the rules, which may name the counters. The counters are checked by
/// the SDK's own rules when a store is given; a rule that names a counter that is not usable is
/// invalid.
pub fn setup(config: &Config, store: &Store) -> Setup {
    let (counters, counters_problem) = match counter_defs(&config.counters).and_then(|defs| {
        Counters::new(store, &defs)
            .map(|_| defs)
            .map_err(|error| Problem::Counters(error.to_string()))
    }) {
        Ok(defs) => (defs, None),
        Err(problem) => (Vec::new(), Some(problem)),
    };
    let rules = parse_rules(&config.rules, &counters);
    let (antiraid, antiraid_problem) = match crate::antiraid::read(config.antiraid.as_ref()) {
        Ok(antiraid) => (antiraid, None),
        Err(problem) => (None, Some(problem)),
    };
    Setup {
        rules,
        counters,
        counters_problem,
        antiraid,
        antiraid_problem,
    }
}

/// The counter definitions of the configuration, read from their written form.
pub fn counter_defs(
    counters: &BTreeMap<String, CounterConfig>,
) -> Result<Vec<CounterDef>, Problem> {
    let invalid = Problem::Key("problem-counters");
    if counters.len() > MAX_COUNTERS {
        return Err(invalid);
    }
    counters
        .iter()
        .map(|(name, config)| {
            if !valid_counter_name(name) || config.triggers.len() > MAX_COUNTER_TRIGGERS {
                return Err(invalid.clone());
            }
            let decay = match &config.decay {
                Some(decay) if decay.every.as_secs() == 0 => return Err(invalid.clone()),
                Some(decay) => Some(Decay {
                    amount: decay.amount,
                    every: decay.every,
                }),
                None => None,
            };
            let triggers = config
                .triggers
                .iter()
                .map(|(trigger, written)| {
                    let (condition, reverse) = match written {
                        TriggerConfig::Short(text) => {
                            let condition = parse_condition(text)?;
                            (condition, condition.reverse())
                        }
                        TriggerConfig::Full(full) => {
                            let condition = parse_condition(&full.condition)?;
                            let reverse = match &full.reverse_condition {
                                Some(text) => parse_condition(text)?,
                                None => condition.reverse(),
                            };
                            (condition, reverse)
                        }
                    };
                    Ok(TriggerDef {
                        name: trigger.clone(),
                        condition,
                        reverse,
                    })
                })
                .collect::<Result<Vec<_>, Problem>>()?;
            Ok(CounterDef {
                name: name.clone(),
                per_channel: config.per_channel,
                per_user: config.per_user,
                initial_value: config.initial_value,
                decay,
                triggers,
            })
        })
        .collect()
}

fn parse_condition(text: &str) -> Result<Condition, Problem> {
    Condition::parse(text).map_err(|_| Problem::Key("problem-counters"))
}

/// Counter names are kept to plain characters, as rule ids are.
fn valid_counter_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Reads the rules one by one. A rule that is not valid is kept as [`Entry::Invalid`] rather than
/// refusing the whole list.
pub fn parse_rules(raw: &BTreeMap<String, Value>, counters: &[CounterDef]) -> Vec<Entry> {
    raw.iter()
        .enumerate()
        .map(|(index, (id, value))| {
            if index >= MAX_RULES {
                return Entry::Invalid {
                    label: id.clone(),
                    why: Problem::Key("problem-too-many"),
                };
            }
            match read(id, value.clone(), counters) {
                Ok(rule) => Entry::Valid(rule),
                Err(why) => Entry::Invalid {
                    label: id.clone(),
                    why,
                },
            }
        })
        .collect()
}

fn read(id: &str, value: Value, counters: &[CounterDef]) -> Result<Rule, Problem> {
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
        trigger.prepare(counters)?;
    }
    if rule.actions.is_empty() || rule.actions.len() > MAX_ACTIONS {
        return Err(Problem::Key("problem-actions"));
    }
    for action in &rule.actions {
        action.validate(counters)?;
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
    fn prepare(&mut self, counters: &[CounterDef]) -> Result<(), Problem> {
        match self {
            Self::AnyMessage(_)
            | Self::MemberLeave(_)
            | Self::ThreadCreated(_)
            | Self::ThreadArchived(_)
            | Self::ThreadUnarchived(_)
            | Self::ThreadDeleted(_)
            | Self::Case(_)
            | Self::AntiraidLevel(_) => Ok(()),
            Self::Words(words) => words.check(),
            Self::Regex(patterns) => patterns.compile(),
            Self::Links(links) => links.compile(),
            Self::Invites(invites) => invites.check(),
            Self::Attachments(files) => files.check(),
            Self::Caps(caps) => caps.check(),
            Self::Repeat(repeat) => repeat.check(),
            Self::Stickers(stickers) => stickers.check(),
            Self::Spam(spam) => spam.check(),
            Self::MemberJoin(join) => {
                if join.new_threshold.is_zero() || join.new_threshold > MAX_NEW_THRESHOLD {
                    Err(Problem::Key("problem-new-threshold"))
                } else {
                    Ok(())
                }
            }
            Self::MemberJoinSpam(spam) => {
                let ok = (1..=MAX_AMOUNT).contains(&spam.amount)
                    && !spam.within.is_zero()
                    && spam.within <= MAX_WINDOW;
                if ok {
                    Ok(())
                } else {
                    Err(Problem::Key("problem-window"))
                }
            }
            Self::RoleAdded(list) | Self::RoleRemoved(list) => list.check(),
            Self::Counter(trigger) => {
                let known = counters.iter().any(|def| {
                    def.name == trigger.counter
                        && def.triggers.iter().any(|t| t.name == trigger.trigger)
                });
                if known {
                    Ok(())
                } else {
                    Err(Problem::Key("problem-counter-unknown"))
                }
            }
        }
    }
}

impl RoleList {
    fn check(&self) -> Result<(), Problem> {
        if (1..=MAX_ROLES).contains(&self.roles.len()) {
            Ok(())
        } else {
            Err(Problem::Key("problem-roles"))
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
    fn validate(&self, counters: &[CounterDef]) -> Result<(), Problem> {
        match self {
            Self::AddRoles { roles } | Self::RemoveRoles { roles } => {
                if (1..=MAX_ROLES).contains(&roles.len()) {
                    Ok(())
                } else {
                    Err(Problem::Key("problem-roles"))
                }
            }
            Self::SetNickname { name } => {
                let length = name.chars().count();
                if (1..=MAX_NICKNAME_CHARS).contains(&length) {
                    reason_ok(Some(name))
                } else {
                    Err(Problem::Key("problem-nickname"))
                }
            }
            Self::Slowmode { duration, channels } => {
                if *duration > MAX_SLOWMODE || channels.len() > MAX_CHANNELS {
                    Err(Problem::Key("problem-slowmode"))
                } else {
                    Ok(())
                }
            }
            Self::AddToCounter { counter, amount } => {
                if !counter_known(counters, counter) {
                    return Err(Problem::Key("problem-counter-unknown"));
                }
                if amount.abs() <= MAX_COUNTER_VALUE {
                    Ok(())
                } else {
                    Err(Problem::Key("problem-counter-amount"))
                }
            }
            Self::SetCounter { counter, value } => {
                if !counter_known(counters, counter) {
                    return Err(Problem::Key("problem-counter-unknown"));
                }
                if (0..=MAX_COUNTER_VALUE).contains(value) {
                    Ok(())
                } else {
                    Err(Problem::Key("problem-counter-amount"))
                }
            }
            Self::Delete(_) | Self::Kick { reason: None } | Self::Warn { reason: None } => Ok(()),
            Self::Kick { reason } | Self::Warn { reason } => reason_ok(reason.as_deref()),
            Self::SetAntiraidLevel { duration, .. } => match duration {
                Some(duration) if duration.is_zero() || *duration > MAX_TIMEOUT => {
                    Err(Problem::Key("problem-antiraid-duration"))
                }
                _ => Ok(()),
            },
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

fn counter_known(counters: &[CounterDef], name: &str) -> bool {
    counters.iter().any(|def| def.name == name)
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
        Trigger::MemberJoin(_) => "trigger-member-join",
        Trigger::MemberJoinSpam(_) => "trigger-member-join-spam",
        Trigger::MemberLeave(_) => "trigger-member-leave",
        Trigger::RoleAdded(_) => "trigger-role-added",
        Trigger::RoleRemoved(_) => "trigger-role-removed",
        Trigger::Case(_) => "trigger-case",
        Trigger::ThreadCreated(_) => "trigger-thread-created",
        Trigger::ThreadArchived(_) => "trigger-thread-archived",
        Trigger::ThreadUnarchived(_) => "trigger-thread-unarchived",
        Trigger::ThreadDeleted(_) => "trigger-thread-deleted",
        Trigger::Counter(_) => "trigger-counter",
        Trigger::AntiraidLevel(_) => "trigger-antiraid-level",
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
        Action::AddRoles { .. } => "action-add-roles",
        Action::RemoveRoles { .. } => "action-remove-roles",
        Action::SetNickname { .. } => "action-set-nickname",
        Action::Slowmode { .. } => "action-slowmode",
        Action::AddToCounter { .. } => "action-add-to-counter",
        Action::SetCounter { .. } => "action-set-counter",
        Action::SetAntiraidLevel { .. } => "action-set-antiraid-level",
    }
}

/// The events a trigger listens to, as the status names them.
pub fn event_key(trigger: &Trigger) -> &'static str {
    match trigger {
        Trigger::AnyMessage(_)
        | Trigger::Words(_)
        | Trigger::Regex(_)
        | Trigger::Links(_)
        | Trigger::Invites(_)
        | Trigger::Attachments(_)
        | Trigger::Caps(_)
        | Trigger::Repeat(_)
        | Trigger::Stickers(_)
        | Trigger::Spam(_) => "event-messages",
        Trigger::MemberJoin(_) | Trigger::MemberJoinSpam(_) => "event-joins",
        Trigger::MemberLeave(_) => "event-leaves",
        Trigger::RoleAdded(_) | Trigger::RoleRemoved(_) => "event-roles",
        Trigger::Case(_) => "event-cases",
        Trigger::ThreadCreated(_)
        | Trigger::ThreadArchived(_)
        | Trigger::ThreadUnarchived(_)
        | Trigger::ThreadDeleted(_) => "event-threads",
        Trigger::Counter(_) => "event-counters",
        Trigger::AntiraidLevel(_) => "event-antiraid",
    }
}

fn hour() -> Duration {
    Duration::from_secs(3_600)
}
