//! Punishing members: the Discord action, the case, the log post, the direct message and the timers
//! of a warning, a timeout, a kick, a ban or a voice mute, and of their lifts. A module that acts
//! without a moderator (automod, as the bot) punishes through this, as a moderator's command does.
//!
//! The caller checks what its own users may do, and reads its settings into [`Settings`]. This
//! module reads no configuration. The module needs the capabilities of what it does: `Cases`,
//! `SendMessages` (the log), `SendDirectMessages`, `KickMembers`, `BanMembers`, `TimeoutMembers`,
//! `ManageRoles`, `MuteMembers`, `ReadGuild` (checking a member) and `Schedule` for the timers,
//! which [`jobs`] registers.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::dm::{self, DirectMessage, Sent};
use crate::duration::format_in;
use crate::embed::escape;
use crate::modlog;

mod roles;

use crate::{
    Args, Case, CaseError, CaseKind, Cases, DiscordError, EventContext, JobCx, Locale,
    ModuleBuilder, ModuleError, NewCase, Reason, RoleId, ScheduleError, UserError, UserId,
    UserInfo,
};
pub use roles::{RestrictionRole, RoleError, channel_created, ensure_role};
use roles::{restore, strip};

/// Discord's longest timeout, 28 days in seconds.
const MAX_TIMEOUT_SECONDS: u32 = 28 * 86_400;
/// The most messages a ban deletes, 7 days in seconds.
const MAX_BAN_DELETE_SECONDS: u32 = 7 * 86_400;
const DAY: i64 = 86_400;
/// How long a voice unmute that Discord refuses waits before it is tried again.
const POSTPONE: Duration = Duration::from_secs(600);
/// How many cases a page of [`Cases::for_target`] holds at most.
const PAGE: usize = 25;
/// How many cases back an active warning is looked for. shortcut: a member with more cases than
/// this has the older warnings left out of the count, upgrade with an index on the target.
const SCAN: usize = 100;
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
            timeout: Method::Discord,
            timeout_role: None,
            voice: Method::Discord,
            voice_role: None,
            strip: Strip::default(),
        }
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
        }
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
                "punish-dm-warned",
                &Args::new()
                    .with("reason", dm_reason(self.ctx, reason.as_deref()))
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
        let now = self.ctx.now();
        let reason_text = dm_reason(self.ctx, reason.as_deref());
        let role = self
            .give_timeout(target, duration, reason.as_deref())
            .await?;
        let case = self
            .record("timeout", target, reason, Some(now + secs(duration)))
            .await?;
        let timer = match role {
            Some(role) => self
                .schedule_timeout_lift(target, case.number, role, duration)
                .await
                .is_err()
                .then_some(Note::NoTimeoutTimer),
            None => None,
        };
        let stripped = self
            .strip_for(target, role, RestrictionRole::Timeout)
            .await?;
        let note = self
            .notify(
                target,
                "punish-dm-timed-out",
                &Args::new()
                    .with("duration", format_in(self.ctx, self.ctx.locale, duration))
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
        if let Some(undone) = self.superseded(target, "timeout").await? {
            return Ok(undone);
        }
        let guild = self.ctx.guild_id;
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
            format!("removed by case #{}", case.number),
        )
        .await;
        Ok(case)
    }

    /// Kicks `target`. The direct message comes first: a kicked member shares no server with the bot.
    pub async fn kick(&self, target: UserId, reason: Option<String>) -> Result<Done, UserError> {
        let note = self
            .notify(
                target,
                "punish-dm-kicked",
                &Args::new().with("reason", dm_reason(self.ctx, reason.as_deref())),
            )
            .await;
        self.ctx
            .api
            .kick(self.ctx.guild_id, target, self.audit(reason.as_deref()))
            .await?;
        let case = self.record("kick", target, reason, None).await?;
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
        if delete_message_seconds > MAX_BAN_DELETE_SECONDS {
            return Err(UserError::sdk("punish-ban-delete-days-range"));
        }
        let note = match duration {
            Some(duration) => {
                self.notify(
                    target,
                    "punish-dm-banned-for",
                    &Args::new()
                        .with("duration", format_in(self.ctx, self.ctx.locale, duration))
                        .with("reason", dm_reason(self.ctx, reason.as_deref())),
                )
                .await
            }
            None => {
                self.notify(
                    target,
                    "punish-dm-banned",
                    &Args::new().with("reason", dm_reason(self.ctx, reason.as_deref())),
                )
                .await
            }
        };
        self.ctx
            .api
            .ban(
                self.ctx.guild_id,
                target,
                delete_message_seconds,
                self.audit(reason.as_deref()),
            )
            .await?;
        let expires_at = duration.map(|duration| self.ctx.now() + secs(duration));
        let case = self.record("ban", target, reason, expires_at).await?;
        // A new ban replaces the timer of an earlier one, which would lift this ban too early.
        let _ = self.ctx.scheduler().cancel(&unban_key(target)).await;
        // The ban holds even when its timer cannot be set; the note says so.
        let timer = match duration {
            Some(duration) => schedule_unban(self.ctx, target, case.number, duration)
                .await
                .is_err()
                .then_some(Note::NoUnbanTimer),
            None => None,
        };
        Ok(Done {
            case,
            notes: [note, timer].into_iter().flatten().collect(),
        })
    }

    /// Lifts the ban of `target` and records an `unban` case, which revokes the newest ban that
    /// still stands.
    pub async fn unban(&self, target: UserId, reason: Option<String>) -> Result<Case, UserError> {
        if let Some(undone) = self.superseded(target, "ban").await? {
            return Ok(undone);
        }
        self.ctx
            .api
            .unban(self.ctx.guild_id, target, self.audit(reason.as_deref()))
            .await?;
        let case = self.record("unban", target, reason, None).await?;
        // Best effort: the member is unbanned already.
        let _ = self.ctx.scheduler().cancel(&unban_key(target)).await;
        self.revoke_lifted(
            target,
            "ban",
            case.number,
            format!("unbanned by case #{}", case.number),
        )
        .await;
        Ok(case)
    }

    /// Mutes `target` in voice, for `duration` or until lifted. The direct message comes after the mute.
    pub async fn voice_mute(
        &self,
        target: UserId,
        duration: Option<Duration>,
        reason: Option<String>,
    ) -> Result<Done, UserError> {
        let now = self.ctx.now();
        let reason_text = dm_reason(self.ctx, reason.as_deref());
        let role = self.mute_in_voice(target, reason.as_deref()).await?;
        let stripped = self.strip_for(target, role, RestrictionRole::Voice).await?;
        let expires_at = duration.map(|duration| now + secs(duration));
        let case = self.record("vcmute", target, reason, expires_at).await?;
        // A new mute replaces the timer of an earlier one, which would lift this mute too early.
        let _ = self.ctx.scheduler().cancel(&voice_key(target)).await;
        let timer = match duration {
            Some(duration) => self
                .schedule_voice_lift(target, case.number, role, duration)
                .await
                .is_err()
                .then_some(Note::NoVoiceTimer),
            None => None,
        };
        let note = match duration {
            Some(duration) => {
                self.notify(
                    target,
                    "punish-dm-voice-muted-for",
                    &Args::new()
                        .with("duration", format_in(self.ctx, self.ctx.locale, duration))
                        .with("reason", reason_text),
                )
                .await
            }
            None => {
                self.notify(
                    target,
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
        if let Some(undone) = self.superseded(target, "vcmute").await? {
            return Ok(undone);
        }
        let guild = self.ctx.guild_id;
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
            format!("removed by case #{}", case.number),
        )
        .await;
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
        match strip(self.ctx, target, which, role, &self.settings.strip.remove).await {
            Ok(skipped) => Ok((skipped > 0).then_some(Note::RolesNotStripped(skipped))),
            // Nothing was taken off when the strip could not be remembered: logged, not refused.
            Err(error) => {
                tracing::warn!(user = %target, %error, "could not take the roles off a punished member");
                Ok(None)
            }
        }
    }

    /// Gives back what the role punishment took off, but only when the lift is the last standing
    /// case of its kind. A failure is logged: the punishment is lifted either way.
    async fn restore_if_last(
        &self,
        target: UserId,
        which: RestrictionRole,
        kind: &str,
    ) -> Result<(), UserError> {
        let standing = active_cases(self.ctx.cases(), target, kind, self.ctx.now(), 2).await?;
        if standing.len() > 1 {
            return Ok(());
        }
        if let Err(error) = restore(self.ctx, target, which, &self.settings.strip.restore).await {
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
        let case = self
            .ctx
            .cases()
            .create(NewCase {
                kind: CaseKind::new(kind)?,
                target,
                moderator: self.moderator.id,
                reason,
                expires_at,
            })
            .await?;
        let _ = modlog::post(self.ctx, &case).await;
        Ok(case)
    }

    /// Revokes what the lift `lift` undoes: the case that `undoing` names, else the newest case of
    /// `kind` that stands, with `reason`. The undo button has always written `removed by case #N`
    /// for a named case, whatever its kind.
    async fn revoke_lifted(&self, target: UserId, kind: &str, lift: i64, reason: String) {
        match self.undoing {
            Some(number) => {
                let _ = self
                    .ctx
                    .cases()
                    .revoke(
                        number,
                        self.moderator.id,
                        Some(format!("removed by case #{lift}")),
                    )
                    .await;
            }
            None => self.revoke_latest(target, kind, reason).await,
        }
    }

    /// Revokes the newest case of `kind` that still stands against `target`. Best effort: the
    /// Discord action is done already.
    async fn revoke_latest(&self, target: UserId, kind: &str, reason: String) {
        if let Ok(cases) = active_cases(self.ctx.cases(), target, kind, self.ctx.now(), 1).await
            && let Some(case) = cases.first()
        {
            let _ = self
                .ctx
                .cases()
                .revoke(case.number, self.moderator.id, Some(reason))
                .await;
        }
    }

    /// Messages `user` in the server language when the server wants that. Returns the note for the
    /// reply when the message did not arrive. A member who is not in the server gets no note.
    async fn notify(&self, user: UserId, key: &str, args: &Args) -> Option<Note> {
        if !self.settings.dm {
            return None;
        }
        let text = self.ctx.t_core(self.ctx.locale, key, args);
        match dm::send(self.ctx, user, DirectMessage::text(text)).await {
            Ok(Sent::Delivered(_))
            | Err(UserError::Module(ModuleError::Discord(DiscordError::NotFound))) => None,
            Ok(Sent::Closed) | Err(_) => Some(Note::DmNotDelivered),
        }
    }
}

/// The payload of an `unban` job: the member, and the case of the ban it lifts.
#[derive(Debug, Serialize, Deserialize)]
struct Unban {
    user: u64,
    case: i64,
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

/// Whether a case of `kind` still stands at `now`: not revoked, and not past its expiry.
fn stands(case: &Case, kind: &str, now: i64) -> bool {
    case.kind.as_str() == kind
        && case.revoked.is_none()
        && case.expires_at.is_none_or(|at| at > now)
}

/// The cases of `kind` against `target` that still stand, newest first, at most `wanted`. Looks
/// back over at most [`SCAN`] cases.
async fn active_cases(
    cases: &Cases,
    target: UserId,
    kind: &str,
    now: i64,
    wanted: usize,
) -> Result<Vec<Case>, CaseError> {
    let mut found = Vec::new();
    let mut before = None;
    let mut looked = 0;
    while found.len() < wanted && looked < SCAN {
        let page = cases.for_target(target, before, PAGE).await?;
        looked += page.len();
        before = page.last().map(|case| case.number);
        found.extend(page.iter().filter(|case| stands(case, kind, now)).cloned());
        if page.len() < PAGE {
            break;
        }
    }
    found.truncate(wanted);
    Ok(found)
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
        .job("unban", lift_ban)
        .job("untimeout", end_timeout)
        .job("vc_unmute", end_voice_mute)
        .job("restriction_sync", roles::restriction_sync)
}

/// Lifts a timed ban when its time is up. A member who was unbanned by hand is done already, so
/// the job may run twice.
async fn lift_ban(cx: JobCx) -> Result<(), ModuleError> {
    let payload: Unban = cx.payload()?;
    let Some(user) = UserId::new_checked(payload.user) else {
        return Err(ModuleError::other("a ban job names no member"));
    };
    if another_stands(cx.ctx(), user, "ban", payload.case).await? {
        return Ok(());
    }
    let reason = Reason::truncated(&format!("the ban of case #{} ran out", payload.case));
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
    match held_by_others(cx.ctx(), user, "timeout", payload.case).await? {
        Hold::Free => {}
        Hold::Forever => return Ok(()),
        Hold::Until(at) => {
            return reschedule(cx.ctx(), "untimeout", &untimeout_key(user), &payload, at).await;
        }
    }
    let reason = Reason::truncated(&format!("the timeout of case #{} ran out", payload.case));
    match cx
        .api()
        .remove_role(cx.guild_id(), user, role, reason)
        .await
    {
        Ok(()) | Err(DiscordError::NotFound) => {
            restore(cx.ctx(), user, RestrictionRole::Timeout, &payload.restore).await?;
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
                .set_voice_mute(cx.guild_id(), user, false, reason)
                .await
        }
        (Method::Role, Some(role)) => {
            cx.api()
                .remove_role(cx.guild_id(), user, role, reason)
                .await
        }
        (Method::Role, None) => {
            return Err(ModuleError::other("a voice mute job names no role"));
        }
    };
    if matches!(payload.via, Method::Role) && matches!(result, Ok(()) | Err(DiscordError::NotFound))
    {
        restore(cx.ctx(), user, RestrictionRole::Voice, &payload.restore).await?;
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
