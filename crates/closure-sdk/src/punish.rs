//! Punishing members: the Discord action, the case, the log post, the direct message and the timers
//! of a warning, a timeout, a kick, a ban or a voice mute, and of their lifts. A module that acts
//! without a moderator (automod, as the bot) punishes through this, as a moderator's command does.
//!
//! The caller checks what its own users may do, and reads its settings into [`Settings`]. This
//! module reads no configuration. The module needs the capabilities of what it does: `Cases`,
//! `SendMessages` (the log), `SendDirectMessages`, `KickMembers`, `BanMembers`, `TimeoutMembers`,
//! `ManageRoles`, `MuteMembers`, `ReadGuild` (checking a member) and `Schedule` for the timers,
//! which [`jobs`] registers.

use std::{collections::BTreeMap, time::Duration};

use closure_core::action::{Action, Outcome};
use serde::{Deserialize, Serialize};
use twilight_model::channel::message::Embed;

use crate::dm::{self, DirectMessage, Sent};
use crate::duration::{TimeStyle, ends_at, format_in, timestamp};
use crate::embed::{DESCRIPTION_LIMIT, EmbedBuilder, escape, truncate};
use crate::message::MessageTemplate;
use crate::modlog;
use crate::template::{Template, Values};
use closure_core::message::MessageSpec;

mod roles;

use crate::{
    Args, Case, CaseError, CaseKind, Cases, DiscordError, EventContext, JobCx, Locale,
    ModuleBuilder, ModuleError, NewCase, Reason, RoleId, ScheduleError, UserError, UserId,
    UserInfo,
};
pub use roles::{
    MemberTurn, RestrictionRole, RoleError, channel_created, ensure_role, is_restriction_role,
    member_turn,
};
use roles::{remove_legacy, restore_when_free, strip};

/// Discord's longest timeout, 28 days in seconds.
const MAX_TIMEOUT_SECONDS: u32 = 28 * 86_400;
/// The most messages a ban deletes, 7 days in seconds.
const MAX_BAN_DELETE_SECONDS: u32 = 7 * 86_400;
const DAY: i64 = 86_400;
/// How long a voice unmute that Discord refuses waits before it is tried again.
const POSTPONE: Duration = Duration::from_secs(600);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(300);
const RECONCILE_BATCH: usize = 50;
/// How many times a ban's case is recorded before the ban is reported as unrecorded.
const RECORD_ATTEMPTS: usize = 3;
/// The case number of a ban whose case could not be recorded. No case has it.
const UNRECORDED_CASE: i64 = 0;
/// How many role ids a member's stripped roles are remembered with, at most.
pub(super) const STRIP_LIMIT: usize = 250;

/// How a timeout or a voice mute is given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    /// Discord's own feature.
    #[default]
    Discord,
    /// A role, which a timer takes off again.
    Role,
}

/// Which roles a role punishment takes off a member, or gives back when it ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RoleSelection {
    /// No role.
    #[default]
    None,
    /// Every role that can be taken off: not @everyone, not managed roles, and none at or above the
    /// bot's top role.
    All,
    /// The listed roles, those the member has.
    Some(Vec<RoleId>),
}

/// A role setting as a server writes it: `false`, `true` for every role that can be taken off, or a
/// list of role ids.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum RoleSetting {
    Flag(bool),
    Ids(Vec<String>),
}

impl RoleSetting {
    /// The selection the setting stands for. Ids that are no role id are left out.
    #[must_use]
    pub fn selection(&self) -> RoleSelection {
        match self {
            Self::Flag(false) => RoleSelection::None,
            Self::Flag(true) => RoleSelection::All,
            Self::Ids(ids) => RoleSelection::Some(
                ids.iter()
                    .filter_map(|id| id.parse::<u64>().ok().and_then(RoleId::new_checked))
                    .collect(),
            ),
        }
    }
}

/// What a role punishment does with the roles of the member: `remove` takes them off when the
/// dedicated role is given, and `restore` gives back the ones taken off when the punishment ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Strip {
    pub remove: RoleSelection,
    pub restore: RoleSelection,
}

impl Default for Strip {
    fn default() -> Self {
        Self {
            remove: RoleSelection::None,
            restore: RoleSelection::All,
        }
    }
}

/// What the server chose for the punishments of a module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Whether the member is messaged about the punishment.
    pub dm: bool,
    pub dm_messages: DmMessages,
    pub timeout: Method,
    /// The role a timeout gives, when `timeout` is [`Method::Role`].
    pub timeout_role: Option<RoleId>,
    pub voice: Method,
    /// The role a voice mute gives, when `voice` is [`Method::Role`].
    pub voice_role: Option<RoleId>,
    /// What a role punishment does with the member's other roles.
    pub strip: Strip,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dm: true,
            dm_messages: DmMessages::default(),
            timeout: Method::Discord,
            timeout_role: None,
            voice: Method::Discord,
            voice_role: None,
            strip: Strip::default(),
        }
    }
}

/// What a server wrote for one kind of punishment: a text, or a message with embeds.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DmMessage {
    Text(Template),
    Rich(MessageTemplate),
}

/// The variables a notification may use.
const DM_VARIABLES: [&str; 6] = [
    "user",
    "server",
    "moderator",
    "reason",
    "duration",
    "expires",
];

/// Validated templates for punishment notifications.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DmMessages(BTreeMap<String, DmMessage>);

/// The public schema of the notification templates.
#[must_use]
pub fn dm_messages_schema(description: &str) -> serde_json::Value {
    let entry = serde_json::json!({
        "anyOf": [
            {"type": "string", "maxLength": 1500, "x-ui": {"widget": "textarea"}},
            closure_core::message::schema(),
            {"type": "null"}
        ],
        "x-ui": {"widget": "message", "variables": DM_VARIABLES}
    });
    serde_json::json!({
        "type": "object", "description": description, "x-ui": {"level": "basic"},
        "properties": {"warn": entry, "timeout": entry, "vcmute": entry, "kick": entry, "ban": entry},
        "additionalProperties": false
    })
}

impl<'de> Deserialize<'de> for DmMessages {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Entry {
            Text(String),
            Rich(MessageSpec),
        }
        let messages = BTreeMap::<String, Option<Entry>>::deserialize(deserializer)?;
        let mut parsed = BTreeMap::new();
        for (kind, entry) in messages {
            if !["warn", "timeout", "vcmute", "kick", "ban"].contains(&kind.as_str()) {
                return Err(serde::de::Error::custom("invalid DM template kind"));
            }
            let message = match entry {
                None => continue,
                Some(Entry::Text(text)) => {
                    if text.chars().count() > 1500 {
                        return Err(serde::de::Error::custom("invalid DM template length"));
                    }
                    let template = Template::parse(&text).map_err(serde::de::Error::custom)?;
                    if !template.uses_only(&DM_VARIABLES) {
                        return Err(serde::de::Error::custom("unknown DM template variable"));
                    }
                    DmMessage::Text(template)
                }
                Some(Entry::Rich(spec)) => DmMessage::Rich(
                    MessageTemplate::parse(spec, &DM_VARIABLES)
                        .map_err(serde::de::Error::custom)?,
                ),
            };
            parsed.insert(kind, message);
        }
        Ok(Self(parsed))
    }
}

/// Who does a punishment, as the cases and the audit log name them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moderator {
    pub id: UserId,
    pub name: String,
}

impl Moderator {
    /// The user who ran an interaction.
    #[must_use]
    pub fn of(user: &UserInfo) -> Self {
        Self {
            id: user.id,
            name: user.name.clone(),
        }
    }

    /// The bot itself, named `name`. Fails when the host did not give the bot's id.
    pub fn bot(ctx: &EventContext, name: &str) -> Result<Self, ModuleError> {
        let id = ctx
            .bot()
            .ok_or_else(|| ModuleError::other("the bot's own user id is not known"))?;
        Ok(Self {
            id,
            name: name.to_owned(),
        })
    }
}

/// Something a punishment could not do all of. The punishment itself went through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    /// The direct message did not reach the member.
    DmNotDelivered,
    /// A timed ban has no timer, so it does not end by itself.
    NoUnbanTimer,
    /// A role timeout has no timer, so it does not end by itself.
    NoTimeoutTimer,
    /// A role voice mute has no timer, so it does not end by itself.
    NoVoiceTimer,
    /// Some roles of the member could not be taken off, or given back.
    RolesNotStripped(u32),
}

impl Note {
    /// The note in `locale`.
    #[must_use]
    pub fn text(self, ctx: &EventContext, locale: Locale) -> String {
        match self {
            Self::RolesNotStripped(count) => ctx.t_core(
                locale,
                self.key(),
                &Args::new().with("count", i64::from(count)),
            ),
            _ => ctx.t_core(locale, self.key(), &Args::new()),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::DmNotDelivered => "punish-dm-not-delivered",
            Self::NoUnbanTimer => "punish-ban-no-timer",
            Self::NoTimeoutTimer => "punish-timeout-no-timer",
            Self::NoVoiceTimer => "punish-vc-no-timer",
            Self::RolesNotStripped(_) => "punish-roles-not-stripped",
        }
    }
}

/// What a punishment left behind: its case, and the notes on what could not be done all of.
#[derive(Debug)]
pub struct Done {
    pub case: Case,
    pub notes: Vec<Note>,
}

/// Punishes members as `moderator`, under the server's `settings`. Direct messages and the log
/// speak the server's language; the notes speak the reply language, the server's unless set.
#[derive(Debug)]
pub struct Punisher<'a> {
    ctx: &'a EventContext,
    moderator: Moderator,
    settings: Settings,
    locale: Locale,
    undoing: Option<i64>,
    source: crate::CaseSource,
}

impl<'a> Punisher<'a> {
    #[must_use]
    pub fn new(ctx: &'a EventContext, moderator: Moderator, settings: Settings) -> Self {
        Self {
            ctx,
            moderator,
            settings,
            locale: ctx.locale,
            undoing: None,
            source: crate::CaseSource::Command,
        }
    }

    /// Where the cases it makes came from (a command by default), so that a search by source
    /// finds what automod or a button did.
    #[must_use]
    pub fn source(mut self, source: crate::CaseSource) -> Self {
        self.source = source;
        self
    }

    /// The language of the notes.
    #[must_use]
    pub fn reply_locale(mut self, locale: Locale) -> Self {
        self.locale = locale;
        self
    }

    /// Makes the lift that follows undo the case `case` and no other: it is revoked, where a lift
    /// otherwise revokes the newest case of its kind that stands. An undo button names the case it
    /// undoes, so that an older one is not taken for the newest.
    ///
    /// When a newer case of the kind still stands, the lift does nothing: Discord and the timers
    /// keep the newer punishment in force. The lift then returns `case` as it is, still standing,
    /// and the caller revokes it.
    #[must_use]
    pub fn undoing(mut self, case: i64) -> Self {
        self.undoing = Some(case);
        self
    }

    /// A note in the reply language.
    #[must_use]
    pub fn note_text(&self, note: &Note) -> String {
        note.text(self.ctx, self.locale)
    }

    /// Warns `target`: a `warn` case that expires at `expires_at`, and a direct message that says
    /// how many warnings stand now. Escalating on the count is up to the caller.
    pub async fn warn(
        &self,
        target: UserId,
        reason: Option<String>,
        expires_at: Option<i64>,
    ) -> Result<Done, UserError> {
        let case = self
            .record("warn", target, reason.clone(), expires_at)
            .await?;
        let active = active_cases(self.ctx.cases(), target, "warn", self.ctx.now(), usize::MAX)
            .await?
            .len();
        let note = self
            .notify(
                target,
                "warn",
                "punish-dm-warned",
                &Args::new()
                    .with("reason", dm_reason(self.ctx, reason.as_deref()))
                    .with(
                        "expires",
                        expires_at
                            .map_or_else(String::new, |at| timestamp(at, TimeStyle::LongDateTime)),
                    )
                    .with(
                        "duration",
                        expires_at.map_or_else(String::new, |at| {
                            let seconds =
                                u64::try_from(at.saturating_sub(self.ctx.now())).unwrap_or(0);
                            format_in(self.ctx, self.ctx.locale, Duration::from_secs(seconds))
                        }),
                    )
                    .with("active", count(active)),
            )
            .await;
        Ok(Done {
            case,
            notes: note.into_iter().collect(),
        })
    }

    /// Times `target` out for `duration`. The direct message comes after the timeout.
    pub async fn timeout(
        &self,
        target: UserId,
        duration: Duration,
        reason: Option<String>,
    ) -> Result<Done, UserError> {
        let _turn = member_turn(self.ctx, target).await;
        let reason_text = dm_reason(self.ctx, reason.as_deref());
        let key = restriction_intent_key("timeout", target);
        reconcile_restriction(self.ctx, &key).await?;
        let now = self.ctx.now();
        let role = match self.settings.timeout {
            Method::Role => Some(self.timeout_role()?),
            Method::Discord => {
                timeout_seconds(duration)?;
                None
            }
        };
        self.remember_restriction(
            "timeout",
            target,
            role,
            now,
            Some(now.saturating_add(secs(duration))),
            reason.as_deref(),
        )
        .await?;
        let role = self
            .give_timeout(target, duration, reason.as_deref())
            .await?;
        let recorded = self
            .save_case("timeout", target, reason, Some(now + secs(duration)))
            .await;
        // The timer is set even when the case is not recorded: the timeout holds all the same.
        let timer = match role {
            Some(role) => self
                .schedule_timeout_lift(
                    target,
                    recorded
                        .as_ref()
                        .map_or(UNRECORDED_CASE, |case| case.number),
                    role,
                    duration,
                )
                .await
                .is_err()
                .then_some(Note::NoTimeoutTimer),
            None => None,
        };
        if recorded.is_ok() && timer.is_none() {
            self.ctx.store().delete(&key).await?;
        }
        let case = recorded?;
        let _ = modlog::post(self.ctx, &case).await;
        let stripped = self
            .strip_for(target, role, RestrictionRole::Timeout)
            .await?;
        let note = self
            .notify(
                target,
                "timeout",
                "punish-dm-timed-out",
                &Args::new()
                    .with(
                        "until",
                        ends_at(self.ctx.now(), duration, TimeStyle::ShortDateTime),
                    )
                    .with("duration", format_in(self.ctx, self.ctx.locale, duration))
                    .with(
                        "expires",
                        ends_at(self.ctx.now(), duration, TimeStyle::LongDateTime),
                    )
                    .with("reason", reason_text),
            )
            .await;
        Ok(Done {
            case,
            notes: [note, timer, stripped].into_iter().flatten().collect(),
        })
    }

    /// Lifts the timeout of `target` and records an `untimeout` case, which revokes the newest
    /// timeout that still stands.
    pub async fn lift_timeout(
        &self,
        target: UserId,
        reason: Option<String>,
    ) -> Result<Case, UserError> {
        let _turn = member_turn(self.ctx, target).await;
        reconcile_restriction(self.ctx, &restriction_intent_key("timeout", target)).await?;
        if let Some(undone) = self.superseded(target, "timeout").await? {
            return Ok(undone);
        }
        let guild = self.ctx.guild_id;
        let role = match self.settings.timeout {
            Method::Role => Some(self.timeout_role()?),
            Method::Discord => None,
        };
        let kept = self
            .remember_action(
                "untimeout",
                target,
                reason.as_deref(),
                Some((self.settings.timeout, role)),
            )
            .await?;
        match self.settings.timeout {
            Method::Discord => {
                self.ctx
                    .api
                    .timeout(guild, target, None, self.audit(reason.as_deref()))
                    .await?;
            }
            Method::Role => {
                let role = self.timeout_role()?;
                self.ctx
                    .api
                    .remove_role(guild, target, role, self.audit(reason.as_deref()))
                    .await?;
                remove_legacy(
                    self.ctx,
                    RestrictionRole::Timeout,
                    target,
                    role,
                    self.audit(reason.as_deref()),
                )
                .await?;
                let _ = self.ctx.scheduler().cancel(&untimeout_key(target)).await;
                self.restore_if_last(target, RestrictionRole::Timeout, "timeout")
                    .await?;
            }
        }
        let case = self.record("untimeout", target, reason, None).await?;
        self.revoke_lifted(
            target,
            "timeout",
            case.number,
            self.ctx.t_core(
                self.ctx.locale,
                "reason-removed-by-case",
                &Args::new().with("case", case.number),
            ),
        )
        .await?;
        if kept {
            self.ctx
                .store()
                .delete(&action_intent_key("untimeout", target))
                .await?;
        }
        Ok(case)
    }

    /// Kicks `target`. The direct message comes first: a kicked member shares no server with the bot.
    pub async fn kick(&self, target: UserId, reason: Option<String>) -> Result<Done, UserError> {
        let note = self
            .notify(
                target,
                "kick",
                "punish-dm-kicked",
                &Args::new().with("reason", dm_reason(self.ctx, reason.as_deref())),
            )
            .await;
        let _turn = member_turn(self.ctx, target).await;
        let kept = self
            .remember_action("kick", target, reason.as_deref(), None)
            .await?;
        self.ctx
            .api
            .kick(self.ctx.guild_id, target, self.audit(reason.as_deref()))
            .await?;
        let case = self.record("kick", target, reason, None).await?;
        if kept {
            self.ctx
                .store()
                .delete(&action_intent_key("kick", target))
                .await?;
        }
        Ok(Done {
            case,
            notes: note.into_iter().collect(),
        })
    }

    /// Bans `target`, for good or for `duration`, and deletes `delete_message_seconds` of its
    /// messages (at most seven days). The direct message comes first, as with a kick.
    pub async fn ban(
        &self,
        target: UserId,
        duration: Option<Duration>,
        delete_message_seconds: u32,
        reason: Option<String>,
    ) -> Result<Done, UserError> {
        let _turn = member_turn(self.ctx, target).await;
        if delete_message_seconds > MAX_BAN_DELETE_SECONDS {
            return Err(UserError::sdk("punish-ban-delete-days-range"));
        }
        let key = ban_intent_key(target);
        // A module without storage can still ban for good; it has no intent to keep.
        let mut kept = true;
        match self.ctx.store().entry(&key).await {
            Ok(Some(_)) => reconcile_ban(self.ctx, &key).await?,
            Ok(None) => {}
            Err(closure_core::storage::StorageError::NotGranted) if duration.is_none() => {
                kept = false;
            }
            Err(error) => return Err(error.into()),
        }
        let note = match duration {
            Some(duration) => {
                self.notify(
                    target,
                    "ban",
                    "punish-dm-banned-for",
                    &Args::new()
                        .with(
                            "until",
                            ends_at(self.ctx.now(), duration, TimeStyle::ShortDateTime),
                        )
                        .with("duration", format_in(self.ctx, self.ctx.locale, duration))
                        .with(
                            "expires",
                            ends_at(self.ctx.now(), duration, TimeStyle::LongDateTime),
                        )
                        .with("reason", dm_reason(self.ctx, reason.as_deref())),
                )
                .await
            }
            None => {
                self.notify(
                    target,
                    "ban",
                    "punish-dm-banned",
                    &Args::new().with("reason", dm_reason(self.ctx, reason.as_deref())),
                )
                .await
            }
        };
        let started_at = self.ctx.now();
        let expires_at = duration.map(|duration| started_at.saturating_add(secs(duration)));
        // The intent comes before the ban, for good or not: a stop between the ban and its case
        // is finished from it.
        if kept {
            self.remember_ban(target, reason.as_deref(), started_at, expires_at)
                .await?;
        }
        self.ctx
            .api
            .ban(
                self.ctx.guild_id,
                target,
                delete_message_seconds,
                self.audit(reason.as_deref()),
            )
            .await?;
        let recorded = self.record_ban(target, reason, expires_at).await;
        // A new ban replaces the timer of an earlier one, which would lift this ban too early.
        let _ = self.ctx.scheduler().cancel(&unban_key(target)).await;
        // The ban holds even when its case is not recorded, so its timer is set all the same.
        let timer = match duration {
            Some(_duration) => {
                let case = recorded
                    .as_ref()
                    .map_or(UNRECORDED_CASE, |case| case.number);
                let remaining = Duration::from_secs(
                    u64::try_from(
                        expires_at
                            .unwrap_or(started_at)
                            .saturating_sub(self.ctx.now()),
                    )
                    .unwrap_or_default(),
                );
                match schedule_unban(self.ctx, target, case, remaining).await {
                    Ok(()) => None,
                    Err(error) => {
                        tracing::error!(user = %target, ?error, "a timed ban has no unban timer");
                        Some(Note::NoUnbanTimer)
                    }
                }
            }
            None => None,
        };
        if kept && recorded.is_ok() && timer.is_none() {
            self.ctx.store().delete(&key).await?;
        }
        let case = recorded?;
        let _ = modlog::post(self.ctx, &case).await;
        Ok(Done {
            case,
            notes: [note, timer].into_iter().flatten().collect(),
        })
    }

    /// Saves what a kick or an unban is about to do. A module without storage keeps no intent;
    /// whether one was kept is returned.
    async fn remember_action(
        &self,
        kind: &str,
        target: UserId,
        reason: Option<&str>,
        how: Option<(Method, Option<RoleId>)>,
    ) -> Result<bool, UserError> {
        let case_before = match self.ctx.cases().for_target(target, None, 1).await {
            Ok(cases) => cases.first().map_or(0, |case| case.number),
            Err(CaseError::NotGranted) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let intent = ActionIntent {
            kind: kind.to_owned(),
            target: target.get(),
            moderator: self.moderator.id.get(),
            reason: reason.map(str::to_owned),
            case_before,
            undoing: self.undoing,
            method: how.map(|(method, _)| method),
            role: how.and_then(|(_, role)| role).map(RoleId::get),
        };
        match self
            .ctx
            .store()
            .set(&action_intent_key(kind, target), &intent)
            .await
        {
            Ok(()) => Ok(true),
            Err(closure_core::storage::StorageError::NotGranted) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    async fn remember_ban(
        &self,
        target: UserId,
        reason: Option<&str>,
        started_at: i64,
        expires_at: Option<i64>,
    ) -> Result<(), UserError> {
        let case_before = match self.ctx.cases().for_target(target, None, 1).await {
            Ok(cases) => Some(cases.first().map_or(0, |case| case.number)),
            Err(CaseError::NotGranted) => None,
            Err(error) => return Err(error.into()),
        };
        self.ctx
            .store()
            .set(
                &ban_intent_key(target),
                &BanIntent {
                    target: target.get(),
                    moderator: self.moderator.id.get(),
                    reason: reason.map(str::to_owned),
                    started_at,
                    expires_at,
                    case_before,
                },
            )
            .await?;
        Ok(())
    }

    async fn remember_restriction(
        &self,
        kind: &str,
        target: UserId,
        role: Option<RoleId>,
        started_at: i64,
        expires_at: Option<i64>,
        reason: Option<&str>,
    ) -> Result<(), UserError> {
        let cases = self.ctx.cases().for_target(target, None, 1).await?;
        let intent = RestrictionIntent {
            kind: kind.to_owned(),
            target: target.get(),
            moderator: self.moderator.id.get(),
            reason: reason.map(str::to_owned),
            started_at,
            expires_at,
            case_before: cases.first().map_or(0, |case| case.number),
            method: if kind == "timeout" {
                self.settings.timeout
            } else {
                self.settings.voice
            },
            role: role.map(RoleId::get),
            restore: self.settings.strip.restore.clone(),
            lifting: false,
        };
        self.ctx
            .store()
            .set(&restriction_intent_key(kind, target), &intent)
            .await?;
        Ok(())
    }

    /// Lifts the ban of `target` and records an `unban` case, which revokes the newest ban that
    /// still stands.
    pub async fn unban(&self, target: UserId, reason: Option<String>) -> Result<Case, UserError> {
        let _turn = member_turn(self.ctx, target).await;
        if let Some(undone) = self.superseded(target, "ban").await? {
            return Ok(undone);
        }
        let kept = self
            .remember_action("unban", target, reason.as_deref(), None)
            .await?;
        self.ctx
            .api
            .unban(self.ctx.guild_id, target, self.audit(reason.as_deref()))
            .await?;
        match self.ctx.store().delete(&ban_intent_key(target)).await {
            Ok(_) | Err(closure_core::storage::StorageError::NotGranted) => {}
            Err(error) => return Err(error.into()),
        }
        let case = self.record("unban", target, reason, None).await?;
        // Best effort: the member is unbanned already.
        let _ = self.ctx.scheduler().cancel(&unban_key(target)).await;
        self.revoke_lifted(
            target,
            "ban",
            case.number,
            self.ctx.t_core(
                self.ctx.locale,
                "reason-unbanned-by-case",
                &Args::new().with("case", case.number),
            ),
        )
        .await?;
        if kept {
            self.ctx
                .store()
                .delete(&action_intent_key("unban", target))
                .await?;
        }
        Ok(case)
    }

    /// Mutes `target` in voice, for `duration` or until lifted. The direct message comes after the mute.
    pub async fn voice_mute(
        &self,
        target: UserId,
        duration: Option<Duration>,
        reason: Option<String>,
    ) -> Result<Done, UserError> {
        let _turn = member_turn(self.ctx, target).await;
        let reason_text = dm_reason(self.ctx, reason.as_deref());
        let key = restriction_intent_key("vcmute", target);
        reconcile_restriction(self.ctx, &key).await?;
        let now = self.ctx.now();
        // The intent comes before the mute, for good or not.
        let kept = !matches!(
            self.ctx.store().entry(&key).await,
            Err(closure_core::storage::StorageError::NotGranted)
        ) || duration.is_some();
        if kept {
            let role = match self.settings.voice {
                Method::Role => Some(self.voice_role()?),
                Method::Discord => None,
            };
            self.remember_restriction(
                "vcmute",
                target,
                role,
                now,
                duration.map(|duration| now.saturating_add(secs(duration))),
                reason.as_deref(),
            )
            .await?;
        }
        let role = self.mute_in_voice(target, reason.as_deref()).await?;
        let stripped = self.strip_for(target, role, RestrictionRole::Voice).await?;
        let expires_at = duration.map(|duration| now + secs(duration));
        let recorded = self.save_case("vcmute", target, reason, expires_at).await;
        // A new mute replaces the timer of an earlier one, which would lift this mute too early.
        let _ = self.ctx.scheduler().cancel(&voice_key(target)).await;
        // The timer is set even when the case is not recorded: the mute holds all the same.
        let timer = match duration {
            Some(duration) => self
                .schedule_voice_lift(
                    target,
                    recorded
                        .as_ref()
                        .map_or(UNRECORDED_CASE, |case| case.number),
                    role,
                    duration,
                )
                .await
                .is_err()
                .then_some(Note::NoVoiceTimer),
            None => None,
        };
        if kept && recorded.is_ok() && timer.is_none() {
            self.ctx.store().delete(&key).await?;
        }
        let case = recorded?;
        let _ = modlog::post(self.ctx, &case).await;
        let note = match duration {
            Some(duration) => {
                self.notify(
                    target,
                    "vcmute",
                    "punish-dm-voice-muted-for",
                    &Args::new()
                        .with(
                            "until",
                            ends_at(self.ctx.now(), duration, TimeStyle::ShortDateTime),
                        )
                        .with("duration", format_in(self.ctx, self.ctx.locale, duration))
                        .with(
                            "expires",
                            ends_at(self.ctx.now(), duration, TimeStyle::LongDateTime),
                        )
                        .with("reason", reason_text),
                )
                .await
            }
            None => {
                self.notify(
                    target,
                    "vcmute",
                    "punish-dm-voice-muted",
                    &Args::new().with("reason", reason_text),
                )
                .await
            }
        };
        Ok(Done {
            case,
            notes: [note, timer, stripped].into_iter().flatten().collect(),
        })
    }

    /// Lifts the voice mute of `target` and records a `vcunmute` case, which revokes the newest
    /// mute that still stands.
    pub async fn voice_unmute(
        &self,
        target: UserId,
        reason: Option<String>,
    ) -> Result<Case, UserError> {
        let _turn = member_turn(self.ctx, target).await;
        reconcile_restriction(self.ctx, &restriction_intent_key("vcmute", target)).await?;
        if let Some(undone) = self.superseded(target, "vcmute").await? {
            return Ok(undone);
        }
        let guild = self.ctx.guild_id;
        let role = match self.settings.voice {
            Method::Role => Some(self.voice_role()?),
            Method::Discord => None,
        };
        let kept = self
            .remember_action(
                "vcunmute",
                target,
                reason.as_deref(),
                Some((self.settings.voice, role)),
            )
            .await?;
        match self.settings.voice {
            Method::Discord => {
                self.ctx
                    .api
                    .set_voice_mute(guild, target, false, self.audit(reason.as_deref()))
                    .await?;
            }
            Method::Role => {
                let role = self.voice_role()?;
                self.ctx
                    .api
                    .remove_role(guild, target, role, self.audit(reason.as_deref()))
                    .await?;
                remove_legacy(
                    self.ctx,
                    RestrictionRole::Voice,
                    target,
                    role,
                    self.audit(reason.as_deref()),
                )
                .await?;
                self.restore_if_last(target, RestrictionRole::Voice, "vcmute")
                    .await?;
            }
        }
        // Best effort: the member is unmuted already.
        let _ = self.ctx.scheduler().cancel(&voice_key(target)).await;
        let case = self.record("vcunmute", target, reason, None).await?;
        self.revoke_lifted(
            target,
            "vcmute",
            case.number,
            self.ctx.t_core(
                self.ctx.locale,
                "reason-removed-by-case",
                &Args::new().with("case", case.number),
            ),
        )
        .await?;
        if kept {
            self.ctx
                .store()
                .delete(&action_intent_key("vcunmute", target))
                .await?;
        }
        Ok(case)
    }

    fn audit(&self, reason: Option<&str>) -> Reason {
        audit_reason(&self.moderator.name, reason)
    }

    /// The case `undoing` names, when a newer case of `kind` still stands against `target`: the
    /// lift then leaves Discord and the timers to that newer case. None when the lift goes on.
    async fn superseded(&self, target: UserId, kind: &str) -> Result<Option<Case>, UserError> {
        let Some(number) = self.undoing else {
            return Ok(None);
        };
        let newest = active_cases(self.ctx.cases(), target, kind, self.ctx.now(), 1).await?;
        if newest.first().is_none_or(|case| case.number <= number) {
            return Ok(None);
        }
        let case = self
            .ctx
            .cases()
            .get(number)
            .await?
            .ok_or(CaseError::NotFound)?;
        Ok(Some(case))
    }

    /// Takes the member's roles off as the settings say, when the dedicated role was given.
    /// Returns the note of the roles that stayed on.
    async fn strip_for(
        &self,
        target: UserId,
        role: Option<RoleId>,
        which: RestrictionRole,
    ) -> Result<Option<Note>, UserError> {
        let Some(role) = role else {
            return Ok(None);
        };
        let configured: Vec<RoleId> = [self.settings.timeout_role, self.settings.voice_role]
            .into_iter()
            .flatten()
            .collect();
        match strip(
            self.ctx,
            target,
            which,
            role,
            &configured,
            &self.settings.strip.remove,
        )
        .await
        {
            Ok(skipped) => Ok((skipped > 0).then_some(Note::RolesNotStripped(skipped))),
            // Nothing was taken off when the strip could not be remembered: logged, not refused.
            Err(error) => {
                tracing::warn!(user = %target, %error, "could not take the roles off a punished member");
                Ok(None)
            }
        }
    }

    /// Gives back what the role punishments took off, but only when no restriction of any kind is
    /// left for the member. A failure is logged: the punishment is lifted either way.
    async fn restore_if_last(
        &self,
        target: UserId,
        which: RestrictionRole,
        kind: &str,
    ) -> Result<(), UserError> {
        // The lift takes away the one standing case of its kind, if there is one.
        let lifting = active_cases(self.ctx.cases(), target, kind, self.ctx.now(), 1)
            .await?
            .len();
        if let Err(error) = restore_when_free(
            self.ctx,
            target,
            which,
            lifting,
            &self.settings.strip.restore,
        )
        .await
        {
            tracing::warn!(user = %target, %error, "could not give back the roles of a lifted punishment");
        }
        Ok(())
    }

    fn timeout_role(&self) -> Result<RoleId, UserError> {
        self.settings
            .timeout_role
            .ok_or_else(|| UserError::sdk("punish-timeout-role-missing"))
    }

    fn voice_role(&self) -> Result<RoleId, UserError> {
        self.settings
            .voice_role
            .ok_or_else(|| UserError::sdk("punish-voice-role-missing"))
    }

    /// Gives a timeout by Discord or by the timeout role. Returns the role a timer has to take off.
    async fn give_timeout(
        &self,
        target: UserId,
        duration: Duration,
        reason: Option<&str>,
    ) -> Result<Option<RoleId>, UserError> {
        let guild = self.ctx.guild_id;
        match self.settings.timeout {
            Method::Discord => {
                let seconds = timeout_seconds(duration)?;
                self.ctx
                    .api
                    .timeout(guild, target, Some(seconds), self.audit(reason))
                    .await?;
                Ok(None)
            }
            Method::Role => {
                let role = self.timeout_role()?;
                self.ctx
                    .api
                    .add_role(guild, target, role, self.audit(reason))
                    .await?;
                Ok(Some(role))
            }
        }
    }

    /// Mutes `target` in voice by Discord or by the voice role. Returns the role a timer takes off.
    async fn mute_in_voice(
        &self,
        target: UserId,
        reason: Option<&str>,
    ) -> Result<Option<RoleId>, UserError> {
        let guild = self.ctx.guild_id;
        match self.settings.voice {
            Method::Discord => {
                self.ctx
                    .api
                    .set_voice_mute(guild, target, true, self.audit(reason))
                    .await?;
                Ok(None)
            }
            Method::Role => {
                let role = self.voice_role()?;
                self.ctx
                    .api
                    .add_role(guild, target, role, self.audit(reason))
                    .await?;
                Ok(Some(role))
            }
        }
    }

    /// Schedules the lift of a role that is given again on rejoin, for the time the case has left. The
    /// timer that lifted the role before is replaced: it may have lifted another role.
    pub async fn rearm_timeout_lift(
        &self,
        target: UserId,
        case: i64,
        role: RoleId,
        after: Duration,
    ) -> Result<(), ScheduleError> {
        self.schedule_timeout_lift(target, case, role, after).await
    }

    /// Schedules the lift of a voice mute role that is given again on rejoin, for the time left.
    pub async fn rearm_voice_lift(
        &self,
        target: UserId,
        case: i64,
        role: RoleId,
        after: Duration,
    ) -> Result<(), ScheduleError> {
        let _ = self.ctx.scheduler().cancel(&voice_key(target)).await;
        self.schedule_voice_lift(target, case, Some(role), after)
            .await
    }

    async fn schedule_timeout_lift(
        &self,
        target: UserId,
        case: i64,
        role: RoleId,
        after: Duration,
    ) -> Result<(), ScheduleError> {
        // A timer set earlier for the member would take the role off too early.
        let _ = self.ctx.scheduler().cancel(&untimeout_key(target)).await;
        let lift = RoleTimeout {
            user: target.get(),
            case,
            role: role.get(),
            restore: self.settings.strip.restore.clone(),
        };
        self.ctx
            .scheduler()
            .job("untimeout", &lift)?
            .key(&untimeout_key(target))
            .after(after)
            .await
    }

    async fn schedule_voice_lift(
        &self,
        target: UserId,
        case: i64,
        role: Option<RoleId>,
        duration: Duration,
    ) -> Result<(), ScheduleError> {
        let lift = VoiceLift {
            user: target.get(),
            case,
            via: self.settings.voice,
            role: role.map(RoleId::get),
            due: self.ctx.now() + secs(duration),
            restore: self.settings.strip.restore.clone(),
        };
        self.ctx
            .scheduler()
            .job("vc_unmute", &lift)?
            .key(&voice_key(target))
            .after(duration)
            .await
    }

    /// Records an action as a case and posts it to the log. A log that cannot take it does not
    /// undo the action.
    async fn record(
        &self,
        kind: &str,
        target: UserId,
        reason: Option<String>,
        expires_at: Option<i64>,
    ) -> Result<Case, UserError> {
        let case = self.save_case(kind, target, reason, expires_at).await?;
        let _ = modlog::post(self.ctx, &case).await;
        Ok(case)
    }

    async fn save_case(
        &self,
        kind: &str,
        target: UserId,
        reason: Option<String>,
        expires_at: Option<i64>,
    ) -> Result<Case, UserError> {
        Ok(self
            .ctx
            .cases()
            .create(NewCase {
                source: self.source,
                kind: CaseKind::new(kind)?,
                target,
                moderator: self.moderator.id,
                reason,
                expires_at,
            })
            .await?)
    }

    /// Revokes what the lift `lift` undoes: the case that `undoing` names, else the newest case of
    /// `kind` that stands, with `reason`. The undo button has always written `removed by case #N`
    /// for a named case, whatever its kind. The Discord action is done already, so a revoke that
    /// fails is logged and returned, not undone.
    async fn revoke_lifted(
        &self,
        target: UserId,
        kind: &str,
        lift: i64,
        reason: String,
    ) -> Result<(), UserError> {
        match self.undoing {
            Some(number) => {
                self.revoke_case(
                    number,
                    self.ctx.t_core(
                        self.ctx.locale,
                        "reason-removed-by-case",
                        &Args::new().with("case", lift),
                    ),
                )
                .await
            }
            None => self.revoke_latest(target, kind, reason).await,
        }
    }

    /// Revokes the case `number` with `reason`. A failure is logged with the case number.
    async fn revoke_case(&self, number: i64, reason: String) -> Result<(), UserError> {
        if let Err(error) = self
            .ctx
            .cases()
            .revoke(number, self.moderator.id, Some(reason))
            .await
        {
            tracing::error!(case = number, %error, "a lifted case could not be revoked");
            return Err(error.into());
        }
        Ok(())
    }

    /// Revokes the newest case of `kind` that still stands against `target`, if there is one.
    async fn revoke_latest(
        &self,
        target: UserId,
        kind: &str,
        reason: String,
    ) -> Result<(), UserError> {
        let standing = active_cases(self.ctx.cases(), target, kind, self.ctx.now(), 1).await?;
        match standing.first() {
            Some(case) => self.revoke_case(case.number, reason).await,
            None => Ok(()),
        }
    }

    /// Records a ban that Discord has applied already. A recording that fails is tried again, up to
    /// [`RECORD_ATTEMPTS`] times. When it still fails, the error says the ban went through without
    /// its case, and the log records the member and the kind.
    async fn record_ban(
        &self,
        target: UserId,
        reason: Option<String>,
        expires_at: Option<i64>,
    ) -> Result<Case, UserError> {
        let mut attempt = 1;
        loop {
            match self
                .save_case("ban", target, reason.clone(), expires_at)
                .await
            {
                Ok(case) => return Ok(case),
                Err(error) if attempt < RECORD_ATTEMPTS => {
                    tracing::warn!(user = %target, attempt, ?error, "a ban could not be recorded, trying again");
                    attempt += 1;
                }
                Err(error) => {
                    tracing::error!(user = %target, kind = "ban", ?error, "a ban went through but its case could not be recorded");
                    return Err(ModuleError::other(format!(
                        "the ban of {target} went through, but its case could not be recorded: {error:?}"
                    ))
                    .into());
                }
            }
        }
    }

    /// The values a notification template can use.
    async fn template_values(&self, user: UserId, needs_server: bool, args: &Args) -> Values {
        let server = if needs_server {
            self.ctx
                .api
                .get_guild(self.ctx.guild_id)
                .await
                .map_or_else(|_| self.ctx.guild_id.to_string(), |guild| guild.name)
        } else {
            String::new()
        };
        let mut values = Values::new()
            .with("user", format!("<@{user}>"))
            .with("server", server)
            .with("moderator", self.moderator.name.clone());
        for (name, value) in args.iter() {
            if let closure_core::locale::ArgValue::Text(text) = value {
                values = values.with(name, text.clone());
            }
        }
        values
    }

    /// Messages `user` in the server language when the server wants that, as an embed titled with the
    /// kind of the punishment. Returns the note for the reply when the message did not arrive. A
    /// member who is not in the server gets no note.
    async fn notify(&self, user: UserId, kind: &str, key: &str, args: &Args) -> Option<Note> {
        if !self.settings.dm {
            return None;
        }
        let text = match self.settings.dm_messages.0.get(kind) {
            Some(DmMessage::Rich(message)) => {
                let values = self
                    .template_values(user, message.uses("server"), args)
                    .await;
                return match message.render(self.ctx.now(), &values) {
                    Ok((content, embeds)) => {
                        match dm::send(self.ctx, user, DirectMessage { content, embeds }).await {
                            Ok(Sent::Delivered(_))
                            | Err(UserError::Module(ModuleError::Discord(
                                DiscordError::NotFound,
                            ))) => None,
                            Ok(Sent::Closed) | Err(_) => Some(Note::DmNotDelivered),
                        }
                    }
                    Err(error) => {
                        tracing::error!(user = %user, kind, %error, "a direct message could not be built");
                        Some(Note::DmNotDelivered)
                    }
                };
            }
            Some(DmMessage::Text(template)) => {
                let values = self
                    .template_values(user, template.uses("server"), args)
                    .await;
                template.render(&values)
            }
            None => self.ctx.t_core(self.ctx.locale, key, args),
        };
        let embed = match notice(self.ctx, kind, &text) {
            Ok(embed) => embed,
            Err(error) => {
                tracing::error!(user = %user, kind, ?error, "a direct message could not be built");
                return Some(Note::DmNotDelivered);
            }
        };
        match dm::send(self.ctx, user, DirectMessage::embed(embed)).await {
            Ok(Sent::Delivered(_))
            | Err(UserError::Module(ModuleError::Discord(DiscordError::NotFound))) => None,
            Ok(Sent::Closed) | Err(_) => Some(Note::DmNotDelivered),
        }
    }
}

/// The payload of an `unban` job: the member, and the case of the ban it lifts, or
/// [`UNRECORDED_CASE`] when that case could not be recorded.
#[derive(Debug, Serialize, Deserialize)]
struct Unban {
    user: u64,
    case: i64,
}

#[derive(Debug, Serialize, Deserialize)]
struct BanIntent {
    target: u64,
    moderator: u64,
    reason: Option<String>,
    started_at: i64,
    /// `None` for a ban for good.
    expires_at: Option<i64>,
    #[serde(default)]
    case_before: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RestrictionIntent {
    kind: String,
    target: u64,
    moderator: u64,
    reason: Option<String>,
    started_at: i64,
    /// `None` for a mute for good.
    expires_at: Option<i64>,
    case_before: i64,
    method: Method,
    role: Option<u64>,
    restore: RoleSelection,
    #[serde(default)]
    lifting: bool,
}

fn restriction_intent_key(kind: &str, user: UserId) -> String {
    format!("{kind}-intent:{}", user.get())
}

/// A kick or an unban that is on its way: saved before Discord is called, so a stop between the
/// action and its case is finished from it.
#[derive(Debug, Serialize, Deserialize)]
struct ActionIntent {
    kind: String,
    target: u64,
    moderator: u64,
    reason: Option<String>,
    case_before: i64,
    /// The case an undo button names, for a lift.
    undoing: Option<i64>,
    /// How a timeout or a voice mute was given, and with which role, for their lifts.
    #[serde(default)]
    method: Option<Method>,
    #[serde(default)]
    role: Option<u64>,
}

fn action_intent_key(kind: &str, user: UserId) -> String {
    format!("{kind}-intent:{}", user.get())
}

fn ban_intent_key(user: UserId) -> String {
    format!("ban-intent:{}", user.get())
}

/// The payload of an `untimeout` job: the member, the case of the timeout, and its role.
#[derive(Debug, Serialize, Deserialize)]
struct RoleTimeout {
    user: u64,
    case: i64,
    role: u64,
    /// Rows scheduled before roles were stripped have none, and restore none.
    #[serde(default)]
    restore: RoleSelection,
}

/// The payload of a `vc_unmute` job. `due` is when the mute was due: a lift that Discord keeps
/// refusing is given up a day after it.
#[derive(Debug, Serialize, Deserialize)]
struct VoiceLift {
    user: u64,
    case: i64,
    via: Method,
    role: Option<u64>,
    due: i64,
    #[serde(default)]
    restore: RoleSelection,
}

fn unban_key(user: UserId) -> String {
    format!("unban:{}", user.get())
}

fn untimeout_key(user: UserId) -> String {
    format!("untimeout:{}", user.get())
}

fn voice_key(user: UserId) -> String {
    format!("vcunmute:{}", user.get())
}

async fn schedule_unban(
    ctx: &EventContext,
    user: UserId,
    case: i64,
    after: Duration,
) -> Result<(), ScheduleError> {
    ctx.scheduler()
        .job(
            "unban",
            &Unban {
                user: user.get(),
                case,
            },
        )?
        .key(&unban_key(user))
        .after(after)
        .await
}

/// The seconds of a timeout, which Discord takes at most 28 days of.
fn timeout_seconds(duration: Duration) -> Result<u32, UserError> {
    u32::try_from(duration.as_secs())
        .ok()
        .filter(|seconds| *seconds <= MAX_TIMEOUT_SECONDS)
        .ok_or_else(|| UserError::sdk("punish-timeout-too-long"))
}

/// The direct message about a punishment as an embed: the kind of the punishment as its title, in the
/// colour the moderation log gives that kind, and the sentence as its description.
fn notice(ctx: &EventContext, kind: &str, text: &str) -> Result<Embed, UserError> {
    Ok(EmbedBuilder::new()
        .title(modlog::kind_name(ctx, kind))
        .color(modlog::color(kind, false))
        .description(truncate(text, DESCRIPTION_LIMIT))
        .timestamp(ctx.now())
        .build()?)
}

/// The reason of a punishment as a direct message says it: escaped, or the words for no reason.
fn dm_reason(ctx: &EventContext, reason: Option<&str>) -> String {
    match reason {
        Some(text) => escape(text),
        None => ctx.t_core(ctx.locale, "punish-reason-none", &Args::new()),
    }
}

/// The reason the audit log keeps: who did it, then why.
fn audit_reason(who: &str, reason: Option<&str>) -> Reason {
    Reason::truncated(&match reason {
        Some(reason) => format!("{who}: {reason}"),
        None => who.to_owned(),
    })
}

/// A length of time in seconds, as the clock and the cases count it.
fn secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

fn count(len: usize) -> i64 {
    i64::try_from(len).unwrap_or(i64::MAX)
}

/// The cases of `kind` against `target` that still stand at `now`, newest first, at most `wanted`.
/// The search covers every case of the member: a case is not missed for being old.
async fn active_cases(
    cases: &Cases,
    target: UserId,
    kind: &str,
    now: i64,
    wanted: usize,
) -> Result<Vec<Case>, CaseError> {
    cases.standing(target, kind, now, wanted).await
}

/// Whether a case of `kind` other than `case` stands against `user` at the job's time. A ban lift
/// waits for it: a timed ban has its own timer, and a permanent one lifts by a command.
async fn another_stands(
    ctx: &EventContext,
    user: UserId,
    kind: &str,
    case: i64,
) -> Result<bool, ModuleError> {
    let standing = active_cases(ctx.cases(), user, kind, ctx.now(), 2)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(standing.iter().any(|other| other.number != case))
}

/// What holds the member after the lift of one case is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    /// No other case of the kind stands.
    Free,
    /// The latest other standing case ends at this time (unix seconds).
    Until(i64),
    /// Another standing case has no end.
    Forever,
}

/// What the other cases of `kind` against `user` leave to the lift of `case`.
async fn held_by_others(
    ctx: &EventContext,
    user: UserId,
    kind: &str,
    case: i64,
) -> Result<Hold, ModuleError> {
    let standing = active_cases(ctx.cases(), user, kind, ctx.now(), usize::MAX)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let mut latest = None;
    for other in standing.iter().filter(|other| other.number != case) {
        let Some(at) = other.expires_at else {
            return Ok(Hold::Forever);
        };
        latest = Some(latest.map_or(at, |latest: i64| latest.max(at)));
    }
    Ok(latest.map_or(Hold::Free, Hold::Until))
}

/// Sets the lift again for `until`, under `key`, which replaces the job the lift is running from.
async fn reschedule<T: Serialize>(
    ctx: &EventContext,
    name: &str,
    key: &str,
    payload: &T,
    until: i64,
) -> Result<(), ModuleError> {
    let after = Duration::from_secs(u64::try_from(until - ctx.now()).unwrap_or_default());
    ctx.scheduler()
        .job(name, payload)
        .map_err(|error| ModuleError::other(error.to_string()))?
        .key(key)
        .after(after)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))
}

/// Registers the handlers of the timers' jobs: `unban`, `untimeout` and `vc_unmute`. Their names,
/// keys and payloads are the ones moderation writes, so rows scheduled before keep working.
///
/// A lift that a newer case of its kind still holds does not take the punishment off early. A ban
/// lift does nothing then. A role lift is set again for the latest end of the others, or does
/// nothing while one of them has no end; it is due again then, so the last one to end takes the
/// role off.
#[must_use]
pub fn jobs(builder: ModuleBuilder) -> ModuleBuilder {
    builder
        .on(
            crate::Subscription::new(closure_core::event::EventKind::GuildAvailable),
            |ctx, _| async move {
                ctx.scheduler()
                    .job("punish_reconcile", &())
                    .map_err(|error| ModuleError::other(error.to_string()))?
                    .key("punish_reconcile")
                    .every(RECONCILE_INTERVAL, Duration::ZERO)
                    .await
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                Ok(crate::Flow::Continue)
            },
        )
        .job("punish_reconcile", |cx| async move {
            modlog::retry_pending(cx.ctx(), RECONCILE_BATCH)
                .await
                .map_err(|error| ModuleError::other(format!("{error:?}")))?;
            reconcile_intents(cx.ctx()).await
        })
        .job("unban", lift_ban)
        .job("untimeout", end_timeout)
        .job("vc_unmute", end_voice_mute)
        .job("restriction_sync", roles::restriction_sync)
}

async fn reconcile_intents(ctx: &EventContext) -> Result<(), ModuleError> {
    let store = ctx.store();
    let start: usize = store
        .get("punish-reconcile-kind")
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?
        .unwrap_or(0);
    let kinds = [
        "ban",
        "timeout",
        "vcmute",
        "kick",
        "unban",
        "untimeout",
        "vcunmute",
        "softban",
    ];
    let mut remaining = RECONCILE_BATCH;
    for offset in 0..kinds.len() {
        if remaining == 0 {
            break;
        }
        remaining -= reconcile_batch(ctx, kinds[(start + offset) % kinds.len()], remaining).await?;
    }
    store
        .set("punish-reconcile-kind", &((start + 1) % kinds.len()))
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(())
}

async fn reconcile_batch(
    ctx: &EventContext,
    kind: &str,
    limit: usize,
) -> Result<usize, ModuleError> {
    let store = ctx.store();
    let cursor_key = format!("{kind}-reconcile-cursor");
    let base = format!("{kind}-intent:");
    let cursor: Option<String> = store
        .get(&cursor_key)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let mut prefixes = Vec::new();
    if let Some(digits) = cursor
        .as_deref()
        .and_then(|key| key.strip_prefix(&base))
        .filter(|digits| {
            !digits.is_empty() && digits.len() <= 20 && digits.bytes().all(|b| b.is_ascii_digit())
        })
    {
        for next in b'0'..=b'9' {
            prefixes.push(format!("{base}{digits}{}", char::from(next)));
        }
        for index in (0..digits.len()).rev() {
            for next in digits.as_bytes()[index] + 1..=b'9' {
                prefixes.push(format!("{base}{}{}", &digits[..index], char::from(next)));
            }
        }
    }
    prefixes.push(base);
    let mut entries = Vec::new();
    for prefix in prefixes {
        entries = store
            .list(&prefix, limit)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if !entries.is_empty() {
            break;
        }
    }
    let last = entries.last().map(|(key, _)| key.clone());
    let count = entries.len();
    for (key, entry) in entries {
        let result: Result<(), UserError> = async {
            let target: u64 =
                serde_json::from_value(entry.value.get("target").cloned().unwrap_or_default())
                    .map_err(|error| ModuleError::other(error.to_string()))?;
            let user = UserId::new_checked(target)
                .ok_or_else(|| ModuleError::other("invalid intent target"))?;
            let _turn = member_turn(ctx, user).await;
            match kind {
                "ban" => reconcile_ban(ctx, &key).await,
                "kick" | "unban" | "untimeout" | "vcunmute" => reconcile_action(ctx, &key).await,
                "softban" => reconcile_softban(ctx, &key).await,
                _ => reconcile_restriction(ctx, &key).await,
            }
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%key, ?error, "could not reconcile a punishment intent");
        }
    }
    if let Some(last) = last {
        store
            .set(&cursor_key, &last)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    Ok(count)
}

async fn reconcile_ban(ctx: &EventContext, key: &str) -> Result<(), UserError> {
    let Some(intent) = ctx.store().get::<BanIntent>(key).await? else {
        return Ok(());
    };
    let target = UserId::new_checked(intent.target)
        .ok_or_else(|| ModuleError::other("invalid ban intent target"))?;
    let moderator = UserId::new_checked(intent.moderator)
        .ok_or_else(|| ModuleError::other("invalid ban intent moderator"))?;
    let outcome = ctx
        .api
        .execute(Action::ListBans {
            guild_id: ctx.guild_id,
            after: UserId::new_checked(intent.target.saturating_sub(1)),
            limit: 1,
        })
        .await?;
    let Outcome::Bans(bans) = outcome else {
        return Err(ModuleError::other("unexpected ban list outcome").into());
    };
    if !bans.iter().any(|ban| ban.user.id == target) {
        ctx.store().delete(key).await?;
        return Ok(());
    }
    let case_before = intent
        .case_before
        .ok_or_else(|| ModuleError::other("ban intent has no case boundary"))?;
    let mut before = None;
    let case = loop {
        let cases = ctx.cases().for_target(target, before, 100).await?;
        if let Some(case) = cases.iter().find(|case| {
            case.module == ctx.module
                && case.kind.as_str() == "ban"
                && case.moderator == moderator
                && case.reason == intent.reason
                && case.expires_at == intent.expires_at
                && case.number > case_before
        }) {
            break case.clone();
        }
        if cases.len() < 100 || cases.last().is_some_and(|case| case.number <= case_before) {
            break ctx
                .cases()
                .create(NewCase {
                    source: crate::CaseSource::Command,
                    kind: CaseKind::new("ban")?,
                    target,
                    moderator,
                    reason: intent.reason.clone(),
                    expires_at: intent.expires_at,
                })
                .await?;
        }
        before = cases.last().map(|case| case.number);
    };
    // A ban for good has no timer; a timed one gets its timer, or is lifted if its time is up.
    if let Some(at) = intent.expires_at.filter(|at| *at > ctx.now()) {
        let after =
            Duration::from_secs(u64::try_from(at.saturating_sub(ctx.now())).unwrap_or_default());
        schedule_unban(ctx, target, case.number, after).await?;
    } else if intent.expires_at.is_some()
        && !another_stands(ctx, target, "ban", case.number).await?
    {
        let reason = Reason::truncated(&ctx.t_core(
            ctx.locale,
            "reason-ban-of-case-ran-out",
            &Args::new().with("case", case.number),
        ));
        match ctx.api.unban(ctx.guild_id, target, reason).await {
            Ok(()) | Err(DiscordError::NotFound) => {}
            Err(error) => return Err(error.into()),
        }
    }
    ctx.store().delete(key).await?;
    let _ = modlog::post(ctx, &case).await;
    Ok(())
}

/// Whether the action of an intent was done: the member is gone (kick), no longer banned (unban),
/// or no longer restricted (a lift of a timeout or a voice mute).
async fn action_done(
    ctx: &EventContext,
    intent: &ActionIntent,
    target: UserId,
) -> Result<bool, UserError> {
    Ok(
        if matches!(intent.kind.as_str(), "untimeout" | "vcunmute") {
            match ctx
                .api
                .execute(Action::GetMember {
                    guild_id: ctx.guild_id,
                    user_id: target,
                })
                .await
            {
                Ok(Outcome::Member(member)) => {
                    let role = intent.role.and_then(RoleId::new_checked);
                    match (intent.method, role) {
                        (Some(Method::Role), Some(role)) => !member.roles.contains(&role),
                        _ if intent.kind == "untimeout" => member
                            .communication_disabled_until
                            .is_none_or(|until| until.as_secs() <= ctx.now()),
                        _ => !member.mute,
                    }
                }
                // A member who left has nothing left to lift.
                Err(DiscordError::NotFound) => false,
                Err(error) => return Err(error.into()),
                Ok(_) => return Err(ModuleError::other("the member was not read").into()),
            }
        } else if intent.kind == "kick" {
            match ctx
                .api
                .execute(Action::GetMember {
                    guild_id: ctx.guild_id,
                    user_id: target,
                })
                .await
            {
                Err(DiscordError::NotFound) => true,
                Err(error) => return Err(error.into()),
                Ok(_) => false,
            }
        } else {
            let outcome = ctx
                .api
                .execute(Action::ListBans {
                    guild_id: ctx.guild_id,
                    after: UserId::new_checked(intent.target.saturating_sub(1)),
                    limit: 1,
                })
                .await?;
            let Outcome::Bans(bans) = outcome else {
                return Err(ModuleError::other("unexpected ban list outcome").into());
            };
            !bans.iter().any(|ban| ban.user.id == target)
        },
    )
}

/// Saves the intent of a softban before its ban goes out, so a stop between the ban and the unban
/// is finished by lifting the ban. Call it under [`member_turn`] and clear it with
/// [`forget_softban`] once the case is made.
///
/// # Errors
/// When the intent cannot be saved: the softban must not start then.
pub async fn remember_softban(
    ctx: &EventContext,
    moderator: UserId,
    target: UserId,
    reason: Option<&str>,
) -> Result<(), UserError> {
    let case_before = ctx
        .cases()
        .for_target(target, None, 1)
        .await?
        .first()
        .map_or(0, |case| case.number);
    let intent = ActionIntent {
        kind: "softban".to_owned(),
        target: target.get(),
        moderator: moderator.get(),
        reason: reason.map(str::to_owned),
        case_before,
        undoing: None,
        method: None,
        role: None,
    };
    ctx.store()
        .set(&action_intent_key("softban", target), &intent)
        .await?;
    Ok(())
}

/// Drops the intent of a softban that is done, or that never banned.
///
/// # Errors
/// When the store cannot be reached.
pub async fn forget_softban(ctx: &EventContext, target: UserId) -> Result<(), UserError> {
    ctx.store()
        .delete(&action_intent_key("softban", target))
        .await?;
    Ok(())
}

/// Finishes a softban that stopped after its ban: lifts the ban, unless somebody else's ban stands
/// against the member by now, and then makes its case.
async fn reconcile_softban(ctx: &EventContext, key: &str) -> Result<(), UserError> {
    let Some(intent) = ctx.store().get::<ActionIntent>(key).await? else {
        return Ok(());
    };
    let target = UserId::new_checked(intent.target)
        .ok_or_else(|| ModuleError::other("invalid intent target"))?;
    if !action_done(ctx, &intent, target).await? {
        // Still banned: a ban that stands as a case is a punishment of its own, not the softban's.
        let others = active_cases(ctx.cases(), target, "ban", ctx.now(), 1)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if !others.is_empty() {
            ctx.store().delete(key).await?;
            return Ok(());
        }
        ctx.api
            .unban(
                ctx.guild_id,
                target,
                closure_core::action::Reason::truncated(
                    intent.reason.as_deref().unwrap_or("Softban"),
                ),
            )
            .await?;
    }
    reconcile_action(ctx, key).await
}

/// Finishes a kick or an unban that stopped between Discord and its case: when the member is gone
/// (kick) or no longer banned (unban) the action was done, so its case is made if it is missing.
async fn reconcile_action(ctx: &EventContext, key: &str) -> Result<(), UserError> {
    let Some(intent) = ctx.store().get::<ActionIntent>(key).await? else {
        return Ok(());
    };
    let target = UserId::new_checked(intent.target)
        .ok_or_else(|| ModuleError::other("invalid intent target"))?;
    let moderator = UserId::new_checked(intent.moderator)
        .ok_or_else(|| ModuleError::other("invalid intent moderator"))?;
    let done = action_done(ctx, &intent, target).await?;
    if !done {
        ctx.store().delete(key).await?;
        return Ok(());
    }
    let mut before = None;
    let case = loop {
        let cases = ctx.cases().for_target(target, before, 100).await?;
        if let Some(case) = cases.iter().find(|case| {
            case.module == ctx.module
                && case.kind.as_str() == intent.kind
                && case.moderator == moderator
                && case.number > intent.case_before
        }) {
            break case.clone();
        }
        if cases.len() < 100
            || cases
                .last()
                .is_some_and(|case| case.number <= intent.case_before)
        {
            break ctx
                .cases()
                .create(NewCase {
                    source: crate::CaseSource::Command,
                    kind: CaseKind::new(&intent.kind)?,
                    target,
                    moderator,
                    reason: intent.reason.clone(),
                    expires_at: None,
                })
                .await?;
        }
        before = cases.last().map(|case| case.number);
    };
    if let Some(lifted) = match intent.kind.as_str() {
        "unban" => Some("ban"),
        "untimeout" => Some("timeout"),
        "vcunmute" => Some("vcmute"),
        _ => None,
    } {
        let mut punisher = Punisher::new(
            ctx,
            Moderator {
                id: moderator,
                name: String::new(),
            },
            Settings::default(),
        );
        if let Some(number) = intent.undoing {
            punisher = punisher.undoing(number);
        }
        let reason = ctx.t_core(
            ctx.locale,
            if lifted == "ban" {
                "reason-unbanned-by-case"
            } else {
                "reason-removed-by-case"
            },
            &Args::new().with("case", case.number),
        );
        punisher
            .revoke_lifted(target, lifted, case.number, reason)
            .await?;
    }
    ctx.store().delete(key).await?;
    let _ = modlog::post(ctx, &case).await;
    Ok(())
}

async fn reconcile_restriction(ctx: &EventContext, key: &str) -> Result<(), UserError> {
    let mut intent = match ctx.store().get::<RestrictionIntent>(key).await {
        Ok(Some(intent)) => intent,
        Ok(None) | Err(closure_core::storage::StorageError::NotGranted) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let target = UserId::new_checked(intent.target)
        .ok_or_else(|| ModuleError::other("invalid restriction intent target"))?;
    let moderator = UserId::new_checked(intent.moderator)
        .ok_or_else(|| ModuleError::other("invalid restriction intent moderator"))?;
    if !["timeout", "vcmute"].contains(&intent.kind.as_str()) {
        return Err(ModuleError::other("invalid restriction intent kind").into());
    }
    let role = match intent.method {
        Method::Role => Some(
            intent
                .role
                .and_then(RoleId::new_checked)
                .ok_or_else(|| ModuleError::other("invalid restriction intent role"))?,
        ),
        Method::Discord => None,
    };
    let member = match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: target,
        })
        .await
    {
        Ok(Outcome::Member(member)) => member,
        Err(DiscordError::NotFound) => {
            ctx.store().delete(key).await?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
        Ok(_) => return Err(ModuleError::other("the member was not read").into()),
    };
    let present = match (intent.kind.as_str(), intent.method, role) {
        (_, Method::Role, Some(role)) => member.roles.contains(&role),
        ("timeout", Method::Discord, _) => member
            .communication_disabled_until
            .is_some_and(|until| until.as_secs() > ctx.now()),
        ("vcmute", Method::Discord, _) => member.mute,
        _ => false,
    };
    if !present {
        if intent.lifting
            && let Some(role) = role
        {
            let which = if intent.kind == "timeout" {
                RestrictionRole::Timeout
            } else {
                RestrictionRole::Voice
            };
            let reason =
                Reason::truncated(&ctx.t_core(ctx.locale, "punish-reason-none", &Args::new()));
            remove_legacy(ctx, which, target, role, reason).await?;
            restore_when_free(ctx, target, which, 0, &intent.restore).await?;
        }
        ctx.store().delete(key).await?;
        return Ok(());
    }
    let mut before = None;
    let case = loop {
        let cases = ctx.cases().for_target(target, before, 100).await?;
        if let Some(case) = cases
            .iter()
            .find(|case| case.kind.as_str() == intent.kind && case.number > intent.case_before)
        {
            break case.clone();
        }
        if cases.len() < 100
            || cases
                .last()
                .is_some_and(|case| case.number <= intent.case_before)
        {
            break ctx
                .cases()
                .create(NewCase {
                    source: crate::CaseSource::Command,
                    kind: CaseKind::new(&intent.kind)?,
                    target,
                    moderator,
                    reason: intent.reason.clone(),
                    expires_at: intent.expires_at,
                })
                .await?;
        }
        before = cases.last().map(|case| case.number);
    };
    finish_restriction(ctx, key, &mut intent, target, role, &case).await?;
    ctx.store().delete(key).await?;
    let _ = modlog::post(ctx, &case).await;
    Ok(())
}

async fn finish_restriction(
    ctx: &EventContext,
    key: &str,
    intent: &mut RestrictionIntent,
    target: UserId,
    role: Option<RoleId>,
    case: &Case,
) -> Result<(), UserError> {
    let settings = Settings {
        voice: intent.method,
        strip: Strip {
            restore: intent.restore.clone(),
            ..Strip::default()
        },
        ..Settings::default()
    };
    let punisher = Punisher::new(
        ctx,
        Moderator {
            id: case.moderator,
            name: String::new(),
        },
        settings,
    );
    // A mute for good has no timer to set.
    let Some(mut due) = intent.expires_at else {
        return Ok(());
    };
    if due <= ctx.now() {
        match held_by_others(ctx, target, &intent.kind, case.number).await? {
            Hold::Forever => return Ok(()),
            Hold::Until(at) => due = at,
            Hold::Free => {
                if role.is_some() {
                    intent.lifting = true;
                    ctx.store().set(key, &*intent).await?;
                }
                let reason =
                    Reason::truncated(&ctx.t_core(ctx.locale, "punish-reason-none", &Args::new()));
                let result = match (intent.kind.as_str(), role) {
                    (_, Some(role)) => {
                        ctx.api
                            .remove_role(ctx.guild_id, target, role, reason.clone())
                            .await
                    }
                    ("vcmute", None) => {
                        ctx.api
                            .set_voice_mute(ctx.guild_id, target, false, reason.clone())
                            .await
                    }
                    (_, None) => {
                        ctx.api
                            .timeout(ctx.guild_id, target, None, reason.clone())
                            .await
                    }
                };
                match result {
                    Ok(()) | Err(DiscordError::NotFound) => {}
                    Err(error) => return Err(error.into()),
                }
                if let Some(role) = role {
                    let which = if intent.kind == "timeout" {
                        RestrictionRole::Timeout
                    } else {
                        RestrictionRole::Voice
                    };
                    remove_legacy(ctx, which, target, role, reason).await?;
                    restore_when_free(ctx, target, which, 0, &intent.restore).await?;
                }
                return Ok(());
            }
        }
    }
    let after =
        Duration::from_secs(u64::try_from(due.saturating_sub(ctx.now())).unwrap_or_default());
    if intent.kind == "vcmute" {
        punisher
            .schedule_voice_lift(target, case.number, role, after)
            .await?;
    } else if let Some(role) = role {
        punisher
            .schedule_timeout_lift(target, case.number, role, after)
            .await?;
    }
    Ok(())
}

/// Lifts a timed ban when its time is up. A member who was unbanned by hand is done already, so
/// the job may run twice.
async fn lift_ban(cx: JobCx) -> Result<(), ModuleError> {
    let payload: Unban = cx.payload()?;
    let Some(user) = UserId::new_checked(payload.user) else {
        return Err(ModuleError::other("a ban job names no member"));
    };
    let _turn = member_turn(cx.ctx(), user).await;
    let key = ban_intent_key(user);
    match cx.ctx().store().entry(&key).await {
        Ok(Some(_)) => {
            reconcile_ban(cx.ctx(), &key)
                .await
                .map_err(|error| ModuleError::other(format!("{error:?}")))?;
            return Ok(());
        }
        Ok(None) | Err(closure_core::storage::StorageError::NotGranted) => {}
        Err(error) => return Err(ModuleError::other(error.to_string())),
    }
    if another_stands(cx.ctx(), user, "ban", payload.case).await? {
        return Ok(());
    }
    let reason = Reason::truncated(&match payload.case {
        UNRECORDED_CASE => cx
            .ctx()
            .t_core(cx.ctx().locale, "reason-ban-ran-out", &Args::new()),
        case => cx.ctx().t_core(
            cx.ctx().locale,
            "reason-ban-of-case-ran-out",
            &Args::new().with("case", case),
        ),
    });
    match cx.api().unban(cx.guild_id(), user, reason).await {
        Ok(()) | Err(DiscordError::NotFound) => Ok(()),
        Err(error) => Err(ModuleError::Discord(error)),
    }
}

/// Takes the timeout role off when its time is up. A member or role that is gone is done already.
async fn end_timeout(cx: JobCx) -> Result<(), ModuleError> {
    let payload: RoleTimeout = cx.payload()?;
    let (Some(user), Some(role)) = (
        UserId::new_checked(payload.user),
        RoleId::new_checked(payload.role),
    ) else {
        return Err(ModuleError::other("a timeout job names no member or role"));
    };
    let _turn = member_turn(cx.ctx(), user).await;
    let key = restriction_intent_key("timeout", user);
    match cx.ctx().store().entry(&key).await {
        Ok(Some(_)) => {
            return reconcile_restriction(cx.ctx(), &key)
                .await
                .map_err(|error| ModuleError::other(format!("{error:?}")));
        }
        Ok(None) | Err(closure_core::storage::StorageError::NotGranted) => {}
        Err(error) => return Err(ModuleError::other(error.to_string())),
    }
    match held_by_others(cx.ctx(), user, "timeout", payload.case).await? {
        Hold::Free => {}
        Hold::Forever => return Ok(()),
        Hold::Until(at) => {
            return reschedule(cx.ctx(), "untimeout", &untimeout_key(user), &payload, at).await;
        }
    }
    let reason = Reason::truncated(&cx.ctx().t_core(
        cx.ctx().locale,
        "reason-timeout-of-case-ran-out",
        &Args::new().with("case", payload.case),
    ));
    match cx
        .api()
        .remove_role(cx.guild_id(), user, role, reason.clone())
        .await
    {
        Ok(()) | Err(DiscordError::NotFound) => {
            remove_legacy(cx.ctx(), RestrictionRole::Timeout, user, role, reason)
                .await
                .map_err(|error| ModuleError::other(error.to_string()))?;
            // The timer's case has run out: nothing of it stands any more.
            restore_when_free(
                cx.ctx(),
                user,
                RestrictionRole::Timeout,
                0,
                &payload.restore,
            )
            .await?;
            Ok(())
        }
        Err(error) => Err(ModuleError::Discord(error)),
    }
}

/// Lifts a voice mute when its time is up. Discord refuses it while the member is not in voice:
/// the lift is then tried again in ten minutes, but given up a day after the mute was due.
async fn end_voice_mute(cx: JobCx) -> Result<(), ModuleError> {
    let payload: VoiceLift = cx.payload()?;
    let Some(user) = UserId::new_checked(payload.user) else {
        return Err(ModuleError::other("a voice mute job names no member"));
    };
    let _turn = member_turn(cx.ctx(), user).await;
    let key = restriction_intent_key("vcmute", user);
    match cx.ctx().store().entry(&key).await {
        Ok(Some(_)) => {
            return reconcile_restriction(cx.ctx(), &key)
                .await
                .map_err(|error| ModuleError::other(format!("{error:?}")));
        }
        Ok(None) | Err(closure_core::storage::StorageError::NotGranted) => {}
        Err(error) => return Err(ModuleError::other(error.to_string())),
    }
    // Nothing else lifts a Discord mute later, so the lift waits for the others as a role does.
    match held_by_others(cx.ctx(), user, "vcmute", payload.case).await? {
        Hold::Free => {}
        Hold::Forever => return Ok(()),
        Hold::Until(at) => {
            return reschedule(cx.ctx(), "vc_unmute", &voice_key(user), &payload, at).await;
        }
    }
    let reason = Reason::truncated(&format!("the voice mute of case #{} ran out", payload.case));
    let result = match (payload.via, payload.role.and_then(RoleId::new_checked)) {
        (Method::Discord, _) => {
            cx.api()
                .set_voice_mute(cx.guild_id(), user, false, reason.clone())
                .await
        }
        (Method::Role, Some(role)) => {
            cx.api()
                .remove_role(cx.guild_id(), user, role, reason.clone())
                .await
        }
        (Method::Role, None) => {
            return Err(ModuleError::other("a voice mute job names no role"));
        }
    };
    if matches!(payload.via, Method::Role) && matches!(result, Ok(()) | Err(DiscordError::NotFound))
    {
        if let Some(role) = payload.role.and_then(RoleId::new_checked) {
            remove_legacy(cx.ctx(), RestrictionRole::Voice, user, role, reason)
                .await
                .map_err(|error| ModuleError::other(error.to_string()))?;
        }
        restore_when_free(cx.ctx(), user, RestrictionRole::Voice, 0, &payload.restore).await?;
    }
    match result {
        Err(DiscordError::NotInVoice) if cx.now() - payload.due <= DAY => cx
            .scheduler()
            .job("vc_unmute", &payload)
            .map_err(|error| ModuleError::other(error.to_string()))?
            .key(&voice_key(user))
            .after(POSTPONE)
            .await
            .map_err(|error| ModuleError::other(error.to_string())),
        // Done: nothing is left to lift, or the lift has been refused for a day.
        Ok(()) | Err(DiscordError::NotFound | DiscordError::NotInVoice) => Ok(()),
        Err(error) => Err(ModuleError::Discord(error)),
    }
}
