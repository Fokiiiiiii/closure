//! The shared punishment flow of the SDK, run through a module that punishes like a command.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::time::Duration;

use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::CommandOptionValue;
use closure_sdk::punish::{
    self, Done, Method, Moderator, Punisher, RoleSelection, RoleSetting, Settings, Strip,
};
use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{
    Capability, Case, CaseKind, Command, CommandArgs, CommandCx, Locale, ModuleBuilder,
    ModuleError, NewCase, UserError, UserId,
};
use serde::Deserialize;
use serde_json::json;
use twilight_model::guild::{Member, Role};
use twilight_model::id::Id;

const MODERATOR: u64 = 100;
const MEMBER: u64 = 7;
const FREE: u64 = 42;
const DAY: u64 = 86_400;
const BOT: u64 = 9_999;

/// The module's settings in the form a server writes them.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Setup {
    dm: Option<bool>,
    timeout: Method,
    timeout_role: Option<String>,
    voice: Method,
    voice_role: Option<String>,
    strip_remove: Option<RoleSetting>,
    strip_restore: Option<RoleSetting>,
}

impl Setup {
    fn settings(&self) -> Settings {
        let role = |text: &Option<String>| {
            text.as_deref()
                .and_then(|text| text.parse::<u64>().ok())
                .and_then(closure_sdk::RoleId::new_checked)
        };
        Settings {
            dm: self.dm.unwrap_or(true),
            timeout: self.timeout,
            timeout_role: role(&self.timeout_role),
            voice: self.voice,
            voice_role: role(&self.voice_role),
            strip: Strip {
                remove: self
                    .strip_remove
                    .as_ref()
                    .map_or(RoleSelection::None, RoleSetting::selection),
                restore: self
                    .strip_restore
                    .as_ref()
                    .map_or(RoleSelection::All, RoleSetting::selection),
            },
        }
    }
}

#[derive(CommandArgs)]
struct Kick {
    /// Member
    user: UserId,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct TimeOut {
    /// Member
    user: UserId,
    /// How long
    duration: Duration,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct Lift {
    /// Member
    user: UserId,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct Ban {
    /// Member
    user: UserId,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
    /// How long
    duration: Option<Duration>,
    /// Days of messages to delete
    delete_days: Option<i64>,
}

#[derive(CommandArgs)]
struct Mute {
    /// Member
    user: UserId,
    /// How long
    duration: Option<Duration>,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,
}

/// An undo: the member, and the case the lift undoes.
#[derive(CommandArgs)]
struct Undo {
    /// Member
    user: UserId,
    /// The case that is undone
    case: i64,
}

fn setup(cx: &CommandCx, moderator: Moderator) -> Result<Punisher<'_>, UserError> {
    let setup: Setup = cx.config()?;
    Ok(Punisher::new(cx.ctx(), moderator, setup.settings()))
}

/// Says the case and the notes, as a command of a module would.
async fn answer(cx: &CommandCx, punisher: &Punisher<'_>, done: Done) -> Result<(), UserError> {
    let mut lines = vec![format!("case #{}", done.case.number)];
    lines.extend(done.notes.iter().map(|note| punisher.note_text(note)));
    cx.reply(lines.join("\n")).await
}

async fn answer_case(cx: &CommandCx, case: Case) -> Result<(), UserError> {
    cx.reply(format!("case #{}", case.number)).await
}

async fn kick(cx: CommandCx, args: Kick) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    let done = punisher.kick(args.user, args.reason).await?;
    answer(&cx, &punisher, done).await
}

async fn bot_kick(cx: CommandCx, args: Kick) -> Result<(), UserError> {
    let moderator = Moderator::bot(cx.ctx(), "Automod")?;
    let punisher = setup(&cx, moderator)?;
    let done = punisher.kick(args.user, args.reason).await?;
    answer(&cx, &punisher, done).await
}

async fn english_kick(cx: CommandCx, args: Kick) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?.reply_locale(Locale::En);
    let done = punisher.kick(args.user, args.reason).await?;
    answer(&cx, &punisher, done).await
}

async fn timeout(cx: CommandCx, args: TimeOut) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    let done = punisher
        .timeout(args.user, args.duration, args.reason)
        .await?;
    answer(&cx, &punisher, done).await
}

async fn untimeout(cx: CommandCx, args: Lift) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    let case = punisher.lift_timeout(args.user, args.reason).await?;
    answer_case(&cx, case).await
}

async fn ban(cx: CommandCx, args: Ban) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    // Days are written as seconds here, so that a refused day count reaches the SDK.
    let seconds = u32::try_from(args.delete_days.unwrap_or(0) * 86_400).unwrap_or(u32::MAX);
    let done = punisher
        .ban(args.user, args.duration, seconds, args.reason)
        .await?;
    answer(&cx, &punisher, done).await
}

async fn unban(cx: CommandCx, args: Lift) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    let case = punisher.unban(args.user, args.reason).await?;
    answer_case(&cx, case).await
}

async fn warn(cx: CommandCx, args: Lift) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    let expires = cx.now() + i64::try_from(30 * DAY).unwrap_or(i64::MAX);
    let done = punisher.warn(args.user, args.reason, Some(expires)).await?;
    answer(&cx, &punisher, done).await
}

async fn voice_mute(cx: CommandCx, args: Mute) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    let done = punisher
        .voice_mute(args.user, args.duration, args.reason)
        .await?;
    answer(&cx, &punisher, done).await
}

async fn voice_unmute(cx: CommandCx, args: Lift) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    let case = punisher.voice_unmute(args.user, args.reason).await?;
    answer_case(&cx, case).await
}

async fn undo_untimeout(cx: CommandCx, args: Undo) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?.undoing(args.case);
    let case = punisher.lift_timeout(args.user, None).await?;
    answer_case(&cx, case).await
}

async fn undo_unban(cx: CommandCx, args: Undo) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?.undoing(args.case);
    let case = punisher.unban(args.user, None).await?;
    answer_case(&cx, case).await
}

async fn undo_voice_unmute(cx: CommandCx, args: Undo) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?.undoing(args.case);
    let case = punisher.voice_unmute(args.user, None).await?;
    answer_case(&cx, case).await
}

/// A ban whose reason is not checked against the length a command option allows, so that its
/// case cannot be recorded.
#[derive(CommandArgs)]
struct RawBan {
    /// Member
    user: UserId,
    /// Why
    reason: Option<String>,
}

async fn ban_raw(cx: CommandCx, args: RawBan) -> Result<(), UserError> {
    let punisher = setup(&cx, Moderator::of(cx.user()))?;
    let done = punisher.ban(args.user, None, 0, args.reason).await?;
    answer(&cx, &punisher, done).await
}

fn module() -> ModuleBuilder {
    let builder = ModuleBuilder::new("punisher", "Punisher", "0.1.0")
        .unwrap()
        .capability(Capability::Cases)
        .capability(Capability::SendMessages)
        .capability(Capability::SendDirectMessages)
        .capability(Capability::KickMembers)
        .capability(Capability::BanMembers)
        .capability(Capability::TimeoutMembers)
        .capability(Capability::ManageRoles)
        .capability(Capability::MuteMembers)
        .capability(Capability::ReadGuild)
        .capability(Capability::Storage)
        .config(json!({ "type": "object", "properties": {} }), json!({}))
        .command(Command::<Kick>::new("kick", "Kick").handler(kick))
        .command(Command::<Kick>::new("bot_kick", "Kick as the bot").handler(bot_kick))
        .command(
            Command::<Kick>::new("english_kick", "Kick, answer in English").handler(english_kick),
        )
        .command(Command::<TimeOut>::new("timeout", "Time out").handler(timeout))
        .command(Command::<Lift>::new("untimeout", "Lift a timeout").handler(untimeout))
        .command(Command::<Ban>::new("ban", "Ban").handler(ban))
        .command(Command::<Lift>::new("unban", "Lift a ban").handler(unban))
        .command(Command::<Lift>::new("warn", "Warn").handler(warn))
        .command(Command::<Mute>::new("vcmute", "Voice mute").handler(voice_mute))
        .command(Command::<Lift>::new("vcunmute", "Lift a voice mute").handler(voice_unmute))
        .command(
            Command::<Undo>::new("undo_untimeout", "Lift a timeout, undoing a case")
                .handler(undo_untimeout),
        )
        .command(
            Command::<Undo>::new("undo_unban", "Lift a ban, undoing a case").handler(undo_unban),
        )
        .command(
            Command::<Undo>::new("undo_vcunmute", "Lift a voice mute, undoing a case")
                .handler(undo_voice_unmute),
        )
        .command(Command::<RawBan>::new("ban_raw", "Ban, reason unchecked").handler(ban_raw));
    punish::jobs(builder)
}

fn harness() -> Harness {
    Harness::new(module()).unwrap()
}

/// A module that can ban but has no cases: a ban goes through, and its case cannot be recorded.
fn module_without_cases() -> ModuleBuilder {
    let builder = ModuleBuilder::new("no_cases", "No cases", "0.1.0")
        .unwrap()
        .capability(Capability::BanMembers)
        .capability(Capability::Schedule)
        .config(json!({ "type": "object", "properties": {} }), json!({}))
        .command(Command::<Ban>::new("ban", "Ban").handler(ban));
    punish::jobs(builder)
}

fn target(name: &str, user: u64) -> Invocation {
    Invocation::new(name).option("user", CommandOptionValue::User(Id::new(user)))
}

fn with_reason(invocation: Invocation, reason: Option<&str>) -> Invocation {
    match reason {
        Some(reason) => invocation.option("reason", CommandOptionValue::String(reason.to_owned())),
        None => invocation,
    }
}

fn with_duration(invocation: Invocation, duration: &str) -> Invocation {
    invocation.option("duration", CommandOptionValue::String(duration.to_owned()))
}

fn kick_of(user: u64, reason: Option<&str>) -> Invocation {
    with_reason(target("kick", user), reason)
}

fn ban_of(
    user: u64,
    reason: Option<&str>,
    duration: Option<&str>,
    days: Option<i64>,
) -> Invocation {
    let invocation = with_reason(target("ban", user), reason);
    let invocation = match duration {
        Some(duration) => with_duration(invocation, duration),
        None => invocation,
    };
    match days {
        Some(days) => invocation.option("delete_days", CommandOptionValue::Integer(days)),
        None => invocation,
    }
}

fn mute_of(user: u64, duration: Option<&str>, reason: Option<&str>) -> Invocation {
    let invocation = with_reason(target("vcmute", user), reason);
    match duration {
        Some(duration) => with_duration(invocation, duration),
        None => invocation,
    }
}

fn text(replies: &[Reply]) -> &str {
    replies.first().map_or("", |reply| reply.content.as_str())
}

async fn cases_of(harness: &Harness, user: u64) -> Vec<Case> {
    harness
        .cases()
        .for_target(Id::new(user), None, 25)
        .await
        .unwrap()
}

/// What the module did to Discord, in order. Lookups of members are left out.
fn sequence(harness: &Harness) -> Vec<&'static str> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SendDirectMessage { .. } => Some("dm"),
            Action::Timeout {
                duration_seconds: Some(_),
                ..
            } => Some("timeout"),
            Action::Timeout {
                duration_seconds: None,
                ..
            } => Some("untimeout"),
            Action::Kick { .. } => Some("kick"),
            Action::Ban { .. } => Some("ban"),
            Action::Unban { .. } => Some("unban"),
            Action::AddRole { .. } => Some("role+"),
            Action::RemoveRole { .. } => Some("role-"),
            Action::SetVoiceMute { mute: true, .. } => Some("vcmute"),
            Action::SetVoiceMute { mute: false, .. } => Some("vcunmute"),
            _ => None,
        })
        .collect()
}

fn dm_contents(harness: &Harness) -> Vec<String> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendDirectMessage { content, .. } => content,
            _ => None,
        })
        .collect()
}

fn ban_deletions(harness: &Harness) -> Vec<u32> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::Ban {
                delete_message_seconds,
                ..
            } => Some(*delete_message_seconds),
            _ => None,
        })
        .collect()
}

fn role_changed(harness: &Harness, role: u64, added: bool) -> bool {
    harness.api().actions().iter().any(|action| match action {
        Action::AddRole { role_id, .. } => added && *role_id == Id::new(role),
        Action::RemoveRole { role_id, .. } => !added && *role_id == Id::new(role),
        _ => false,
    })
}

/// Fills the scheduler of the module, so that the next job is refused.
async fn fill_scheduler(harness: &Harness) {
    for n in 0..10_000_u32 {
        harness
            .scheduler()
            .job("filler", &n)
            .unwrap()
            .after(Duration::from_secs(60))
            .await
            .unwrap();
    }
}

// ---- kick ----

#[tokio::test]
async fn a_kick_is_messaged_first_then_kicked_and_recorded_with_the_moderator() {
    let harness = harness();
    harness.set_log_channel(50);
    let replies = harness.invoke(kick_of(MEMBER, Some("rude"))).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["dm", "kick"]);

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "kick");
    assert_eq!(case.target, Id::new(MEMBER));
    assert_eq!(case.moderator, Id::new(MODERATOR));
    assert_eq!(case.reason.as_deref(), Some("rude"));
    assert_eq!(case.expires_at, None);
    assert!(dm_contents(&harness)[0].contains("rude"));

    let kicked = harness
        .api()
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::Kick { reason, .. } => Some(reason.as_str().to_owned()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        kicked, "tester: rude",
        "the audit reason names the moderator"
    );

    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), 1, "{embeds:?}");
    assert_eq!(embeds[0].title.as_deref(), Some("Case #1 · Kick"));
}

#[tokio::test]
async fn a_kick_without_direct_messages_is_only_the_kick() {
    let harness = harness();
    harness.set_config(json!({ "dm": false }));
    harness.invoke(kick_of(MEMBER, Some("rude"))).await.unwrap();
    assert_eq!(sequence(&harness), ["kick"]);
}

#[tokio::test]
async fn a_closed_direct_message_is_a_note_and_the_kick_goes_on() {
    let harness = harness();
    harness
        .api()
        .fail_nth(1, DiscordError::DirectMessagesClosed);
    let replies = harness.invoke(kick_of(MEMBER, Some("rude"))).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(
        text(&replies).contains("could not be delivered"),
        "{replies:?}"
    );
    assert_eq!(sequence(&harness), ["kick"]);
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
}

#[tokio::test]
async fn a_member_who_left_the_server_is_not_a_note() {
    let harness = harness();
    harness.api().fail_nth(1, DiscordError::NotFound);
    let replies = harness.invoke(kick_of(MEMBER, None)).await.unwrap();
    assert!(
        !text(&replies).contains("could not be delivered"),
        "{replies:?}"
    );
    assert!(text(&replies).contains("case #1"), "{replies:?}");
}

#[tokio::test]
async fn a_kick_the_discord_refuses_leaves_no_case_and_no_log() {
    let harness = harness();
    harness.set_log_channel(50);
    // The direct message is the first call, the kick the second.
    harness
        .api()
        .fail_nth(2, DiscordError::Forbidden("missing permission".to_owned()));
    assert!(harness.invoke(kick_of(MEMBER, Some("rude"))).await.is_err());
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert!(harness.sent_embeds().is_empty());
}

// ---- timeout ----

#[tokio::test]
async fn a_timeout_is_applied_before_the_direct_message_and_expires_with_its_case() {
    let harness = harness();
    let replies = harness
        .invoke(with_reason(
            with_duration(target("timeout", MEMBER), "1h"),
            Some("spam"),
        ))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["timeout", "dm"]);
    assert!(harness.api().actions().iter().any(|action| matches!(
        action,
        Action::Timeout {
            duration_seconds: Some(3_600),
            ..
        }
    )));

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "timeout");
    assert_eq!(case.target, Id::new(MEMBER));
    assert_eq!(case.reason.as_deref(), Some("spam"));
    assert_eq!(case.expires_at, Some(case.created_at + 3_600));
    assert!(dm_contents(&harness)[0].contains("1 hour"));
    assert!(dm_contents(&harness)[0].contains("spam"));
    assert_eq!(harness.jobs(), 0, "Discord's timeout has no timer");
}

#[tokio::test]
async fn a_timeout_lasts_at_most_twenty_eight_days_with_discord() {
    let harness = harness();
    let refused = harness
        .invoke(with_duration(target("timeout", MEMBER), "2419201s"))
        .await
        .unwrap();
    assert!(text(&refused).contains("28 days"), "{refused:?}");
    assert!(sequence(&harness).is_empty());
    assert!(cases_of(&harness, MEMBER).await.is_empty());

    harness
        .invoke(with_duration(target("timeout", MEMBER), "28d"))
        .await
        .unwrap();
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
}

#[tokio::test]
async fn a_timeout_the_discord_refuses_leaves_no_case() {
    let harness = harness();
    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("missing permission".to_owned()));
    assert!(
        harness
            .invoke(with_duration(target("timeout", MEMBER), "1h"))
            .await
            .is_err()
    );
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert!(sequence(&harness).is_empty());
}

#[tokio::test]
async fn an_untimeout_lifts_the_timeout_and_revokes_the_one_it_lifts() {
    let harness = harness();
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(with_reason(target("untimeout", MEMBER), Some("sorry")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    assert_eq!(sequence(&harness), ["timeout", "dm", "untimeout"]);

    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "untimeout");
    assert_eq!(lifted.reason.as_deref(), Some("sorry"));
    let timed = harness.cases().get(1).await.unwrap().unwrap();
    let revoked = timed.revoked.expect("the timeout is revoked");
    assert_eq!(revoked.reason.as_deref(), Some("removed by case #2"));
}

#[tokio::test]
async fn an_untimeout_without_a_timeout_still_succeeds() {
    let harness = harness();
    let replies = harness.invoke(target("untimeout", MEMBER)).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["untimeout"]);
}

#[tokio::test]
async fn a_timeout_by_role_adds_the_role_and_a_timer_removes_it_when_due() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    let replies = harness
        .invoke(with_reason(
            with_duration(target("timeout", MEMBER), "1h"),
            Some("spam"),
        ))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm"]);
    assert!(role_changed(&harness, 30, true));
    assert!(
        !harness
            .api()
            .actions()
            .iter()
            .any(|action| matches!(action, Action::Timeout { .. })),
        "Discord's own timeout is not used"
    );
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.expires_at, Some(case.created_at + 3_600));
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_timeout_by_role_does_nothing_without_a_usable_role() {
    let harness = harness();
    for config in [
        json!({ "timeout": "role" }),
        json!({ "timeout": "role", "timeout_role": "0" }),
    ] {
        harness.set_config(config);
        let replies = harness
            .invoke(with_duration(target("timeout", MEMBER), "1h"))
            .await
            .unwrap();
        assert!(text(&replies).contains("timeout_role"), "{replies:?}");
    }
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_timeout_by_role_may_last_longer_than_discord_allows() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    let replies = harness
        .invoke(with_duration(target("timeout", MEMBER), "29d"))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm"]);
    assert_eq!(harness.jobs(), 1);
}

#[tokio::test]
async fn a_timeout_by_role_holds_when_its_timer_cannot_be_set() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    fill_scheduler(&harness).await;
    let replies = harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(text(&replies).contains("timer"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm"]);
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
}

#[tokio::test]
async fn an_untimeout_by_role_removes_the_role_and_cancels_the_timer() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(target("untimeout", MEMBER)).await.unwrap();
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);
    assert_eq!(harness.jobs(), 0, "the timer is cancelled");
}

#[tokio::test]
async fn an_untimeout_by_role_needs_the_role_too() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role" }));
    let replies = harness.invoke(target("untimeout", MEMBER)).await.unwrap();
    assert!(text(&replies).contains("timeout_role"), "{replies:?}");
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn a_timed_out_role_is_the_one_the_timeout_gave_even_after_a_config_change() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "32" }));

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert!(role_changed(&harness, 30, false));
    assert!(!role_changed(&harness, 32, false));
}

fn guild_role(id: u64, position: i64) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(),
        "name": "role",
        "color": 0,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null },
        "hoist": false,
        "managed": false,
        "mentionable": false,
        "permissions": "0",
        "position": position,
        "flags": 0,
    }))
    .unwrap()
}

fn member_with_roles(id: u64, roles: &[u64]) -> Member {
    serde_json::from_value(json!({
        "deaf": false,
        "mute": false,
        "flags": 0,
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "user": {
            "id": id.to_string(),
            "username": "member",
            "discriminator": "0",
            "avatar": null,
            "bot": false,
        },
    }))
    .unwrap()
}

// ---- roles taken off and given back ----

#[tokio::test]
async fn a_lift_that_leaves_a_role_out_does_not_let_a_later_lift_give_it_back() {
    let harness = harness();
    harness
        .api()
        .set_roles(Id::new(1), vec![guild_role(700, 1)]);
    harness
        .api()
        .set_member(Id::new(1), member_with_roles(MEMBER, &[700]));
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30", "strip_remove": true }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    assert!(role_changed(&harness, 700, false), "700 is taken off");

    // This lift gives back only role 999, which the member never had: 700 is left out for good.
    harness.set_config(json!({
        "timeout": "role", "timeout_role": "30", "strip_remove": true, "strip_restore": ["999"]
    }));
    harness.invoke(target("untimeout", MEMBER)).await.unwrap();
    assert!(!role_changed(&harness, 700, true), "nothing is given back");

    // A later punishment takes nothing off, and its lift gives back everything it remembers.
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30", "strip_restore": true }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.invoke(target("untimeout", MEMBER)).await.unwrap();
    assert!(
        !role_changed(&harness, 700, true),
        "700 was left out of the earlier lift and is not given back now"
    );
}

#[tokio::test]
async fn a_role_taken_off_while_a_lift_gives_roles_back_is_given_back_later() {
    let harness = harness();
    let held: Vec<u64> = (700..708).collect();
    let mut known: Vec<Role> = held.iter().map(|id| guild_role(*id, 1)).collect();
    known.push(guild_role(900, 1));
    harness.api().set_roles(Id::new(1), known);
    harness
        .api()
        .set_member(Id::new(1), member_with_roles(MEMBER, &held));
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30", "strip_remove": true }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    assert!(
        held.iter().all(|id| role_changed(&harness, *id, false)),
        "every role is taken off"
    );

    // The member now holds 900 instead. The lift gives the eight roles back one call at a time,
    // and a timeout that starts a few calls later takes 900 off while the lift is still running.
    harness
        .api()
        .set_member(Id::new(1), member_with_roles(MEMBER, &[900]));
    let (lift, punish) = tokio::time::timeout(Duration::from_secs(10), async {
        let later = async {
            // Three calls: the lift has checked for other punishments by then, and is in its restore.
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            harness
                .invoke(with_duration(target("timeout", MEMBER), "1h"))
                .await
        };
        tokio::join!(harness.invoke(target("untimeout", MEMBER)), later)
    })
    .await
    .expect("the lift and the timeout finish");
    lift.unwrap();
    punish.unwrap();
    assert!(
        held.iter().all(|id| role_changed(&harness, *id, true)),
        "the lift gives the roles back"
    );
    assert!(
        role_changed(&harness, 900, false),
        "the timeout takes 900 off"
    );

    harness.invoke(target("untimeout", MEMBER)).await.unwrap();
    assert!(
        role_changed(&harness, 900, true),
        "900 was taken off while the lift ran, and must be given back by a later lift"
    );
}

// ---- ban and unban ----

#[tokio::test]
async fn a_permanent_ban_deletes_no_messages_by_default_and_schedules_nothing() {
    let harness = harness();
    let replies = harness
        .invoke(ban_of(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["dm", "ban"]);
    assert_eq!(ban_deletions(&harness), [0]);
    assert_eq!(harness.jobs(), 0);

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "ban");
    assert_eq!(case.expires_at, None);
}

#[tokio::test]
async fn a_ban_deletes_the_days_it_is_given_and_refuses_more_than_seven() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), None, Some(1)))
        .await
        .unwrap();
    assert_eq!(ban_deletions(&harness), [86_400]);

    let refused = harness
        .invoke(ban_of(FREE, Some("raid"), None, Some(8)))
        .await
        .unwrap();
    assert!(text(&refused).contains("0 to 7"), "{refused:?}");
    assert_eq!(ban_deletions(&harness), [86_400]);
    assert!(cases_of(&harness, FREE).await.is_empty());
}

#[tokio::test]
async fn a_timed_ban_is_lifted_when_its_time_is_up() {
    let harness = harness();
    let replies = harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(dm_contents(&harness)[0].contains("1 day"));
    assert_eq!(harness.jobs(), 1);

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(
        case.expires_at,
        Some(case.created_at + i64::try_from(DAY).unwrap())
    );

    harness.advance(Duration::from_secs(DAY - 1));
    assert!(harness.run_due_jobs().await.is_empty(), "not due yet");
    harness.advance(Duration::from_secs(1));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["dm", "ban", "unban"]);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_timed_ban_lifted_by_hand_is_not_an_error_for_its_lift() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(DAY));
    harness.api().fail_nth(1, DiscordError::NotFound);
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_lift_that_fails_otherwise_is_left_to_run_again() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(DAY));
    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("missing permission".to_owned()));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_err(), "{ended:?}");
    assert_eq!(harness.jobs(), 1, "still waiting to run again");
}

#[tokio::test]
async fn a_ban_the_discord_refuses_leaves_no_case_and_no_timer() {
    let harness = harness();
    // The direct message is the first call, the ban the second.
    harness
        .api()
        .fail_nth(2, DiscordError::Forbidden("missing permission".to_owned()));
    assert!(
        harness
            .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
            .await
            .is_err()
    );
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_timed_ban_holds_even_when_its_timer_cannot_be_set() {
    let harness = harness();
    harness.set_log_channel(50);
    fill_scheduler(&harness).await;
    let replies = harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert!(text(&replies).contains("timer"), "{replies:?}");
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
    assert_eq!(harness.sent_embeds().len(), 1);
    assert_eq!(harness.jobs(), 10_000);
}

#[tokio::test]
async fn a_ban_for_good_that_replaces_a_timed_one_drops_its_timer() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban_of(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 0, "the old timer would lift the new ban");
}

#[tokio::test]
async fn a_timed_ban_is_still_timed_when_its_case_cannot_be_recorded() {
    let harness = Harness::new(module_without_cases()).unwrap();
    assert!(
        harness
            .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
            .await
            .is_err(),
        "the reply says the case could not be recorded"
    );
    assert!(sequence(&harness).contains(&"ban"), "the ban went through");
    assert_eq!(harness.jobs(), 1, "the ban still has its timer");
}

/// The timer of a ban whose case was not recorded lifts the ban when it is due.
#[tokio::test]
async fn a_timer_of_an_unrecorded_ban_lifts_it_when_due() {
    let harness = harness();
    harness
        .scheduler()
        .job("unban", &json!({ "user": MEMBER, "case": 0 }))
        .unwrap()
        .key(&format!("unban:{MEMBER}"))
        .after(Duration::from_secs(60))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(60));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["unban"]);
}

#[tokio::test]
async fn an_unban_lifts_the_ban_cancels_its_timer_and_revokes_the_ban() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 1);
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(with_reason(target("unban", MEMBER), Some("appeal")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    assert_eq!(sequence(&harness), ["dm", "ban", "unban"]);
    assert_eq!(harness.jobs(), 0, "the timer is cancelled");

    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "unban");
    let banned = harness.cases().get(1).await.unwrap().unwrap();
    let revoked = banned.revoked.expect("the ban is revoked");
    assert_eq!(revoked.reason.as_deref(), Some("unbanned by case #2"));
}

#[tokio::test]
async fn an_unban_without_a_ban_still_succeeds() {
    let harness = harness();
    let replies = harness.invoke(target("unban", MEMBER)).await.unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["unban"]);
}

// ---- voice mute ----

#[tokio::test]
async fn a_voice_mute_in_discord_is_recorded_and_messaged_after_it() {
    let harness = harness();
    let replies = harness
        .invoke(mute_of(MEMBER, None, Some("noisy")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm"]);

    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.kind.as_str(), "vcmute");
    assert_eq!(case.expires_at, None);
    assert_eq!(case.reason.as_deref(), Some("noisy"));
    assert_eq!(harness.jobs(), 0, "no duration, no timer");
}

#[tokio::test]
async fn a_voice_mute_with_a_duration_is_lifted_when_its_time_is_up() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    assert!(dm_contents(&harness)[0].contains("1 hour"));
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.expires_at, Some(case.created_at + 3_600));
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm", "vcunmute"]);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_mute_the_discord_refuses_for_a_member_out_of_voice_leaves_no_case() {
    let harness = harness();
    harness.set_log_channel(50);
    harness.api().fail_nth(1, DiscordError::NotInVoice);
    let result = harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await;
    assert!(
        matches!(result, Err(ModuleError::Discord(DiscordError::NotInVoice))),
        "{result:?}"
    );
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert!(harness.sent_embeds().is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_mute_by_role_adds_the_voice_role_and_lifts_it_when_due() {
    let harness = harness();
    harness.set_config(json!({ "voice": "role", "voice_role": "31" }));
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    assert_eq!(sequence(&harness), ["role+", "dm"]);
    assert!(role_changed(&harness, 31, true));
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_secs(3_600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);
}

#[tokio::test]
async fn a_voice_mute_by_role_needs_its_role() {
    let harness = harness();
    harness.set_config(json!({ "voice": "role" }));
    let replies = harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    assert!(text(&replies).contains("voice_mute_role"), "{replies:?}");
    assert!(sequence(&harness).is_empty(), "{:?}", sequence(&harness));
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_unmute_lifts_the_mute_cancels_its_timer_and_revokes_it() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness
        .invoke(with_reason(target("vcunmute", MEMBER), Some("enough")))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm", "vcunmute"]);
    assert_eq!(harness.jobs(), 0, "the timer is cancelled");

    let lifted = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(lifted.kind.as_str(), "vcunmute");
    let muted = harness.cases().get(1).await.unwrap().unwrap();
    let revoked = muted.revoked.expect("the mute is revoked");
    assert_eq!(revoked.reason.as_deref(), Some("removed by case #2"));
}

#[tokio::test]
async fn a_voice_unmute_the_discord_refuses_leaves_the_mute_standing() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, None, Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    // The direct message of the mute is the first call, so the unmute is the second.
    harness.api().fail_nth(1, DiscordError::NotInVoice);
    let result = harness.invoke(target("vcunmute", MEMBER)).await;
    assert!(
        matches!(result, Err(ModuleError::Discord(DiscordError::NotInVoice))),
        "{result:?}"
    );
    let muted = harness.cases().get(1).await.unwrap().unwrap();
    assert!(muted.revoked.is_none());
    assert_eq!(cases_of(&harness, MEMBER).await.len(), 1);
}

#[tokio::test]
async fn a_voice_unmute_by_role_removes_the_role_and_needs_it() {
    let harness = harness();
    harness.set_config(json!({ "voice": "role", "voice_role": "31" }));
    harness
        .invoke(mute_of(MEMBER, None, Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(target("vcunmute", MEMBER)).await.unwrap();
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);

    harness.set_config(json!({ "voice": "role" }));
    harness.advance(Duration::from_secs(3));
    let replies = harness.invoke(target("vcunmute", MEMBER)).await.unwrap();
    assert!(text(&replies).contains("voice_mute_role"), "{replies:?}");
    assert_eq!(sequence(&harness), ["role+", "dm", "role-"]);
}

#[tokio::test]
async fn a_voice_lift_the_discord_refuses_for_now_is_tried_ten_minutes_later() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3_600));
    harness.api().fail_nth(1, DiscordError::NotInVoice);
    let ended = harness.run_due_jobs().await;
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert!(ended[0].is_ok(), "{ended:?}");
    assert_eq!(harness.jobs(), 1, "postponed, not given up");
    assert_eq!(sequence(&harness), ["vcmute", "dm"], "nothing lifted yet");

    harness.advance(Duration::from_secs(599));
    assert!(harness.run_due_jobs().await.is_empty(), "not yet");
    harness.advance(Duration::from_secs(1));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["vcmute", "dm", "vcunmute"]);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_lift_gives_up_a_day_after_it_fell_due() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3_600));
    harness.api().fail_with(DiscordError::NotInVoice);
    let mut runs = 0;
    while harness.jobs() > 0 {
        assert!(runs < 200, "the lift never gives up");
        let ended = harness.run_due_jobs().await;
        assert!(ended.iter().all(Result::is_ok), "{ended:?}");
        runs += 1;
        harness.advance(Duration::from_secs(600));
    }
    // About a day of tries, ten minutes apart, then it is dropped.
    assert!((144..=147).contains(&runs), "{runs} runs");
    let attempts = sequence(&harness);
    assert_eq!(attempts[..2], ["vcmute", "dm"]);
    assert!(
        attempts[2..].iter().all(|step| *step == "vcunmute"),
        "{attempts:?}"
    );
}

#[tokio::test]
async fn a_voice_lift_by_role_is_done_when_the_member_or_role_is_gone() {
    let harness = harness();
    harness.set_config(json!({ "voice": "role", "voice_role": "31" }));
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3_600));
    harness.api().fail_nth(1, DiscordError::NotFound);
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(harness.jobs(), 0);
    assert_eq!(sequence(&harness), ["role+", "dm"]);
}

// ---- warn ----

#[tokio::test]
async fn a_warning_is_a_case_that_expires_and_its_direct_message_counts_the_active_ones() {
    let harness = harness();
    harness.invoke(target("warn", MEMBER)).await.unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(with_reason(target("warn", MEMBER), Some("spam")))
        .await
        .unwrap();

    let warned = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(warned.kind.as_str(), "warn");
    assert_eq!(
        warned.expires_at,
        Some(warned.created_at + i64::try_from(30 * DAY).unwrap())
    );
    let direct = dm_contents(&harness);
    assert_eq!(direct.len(), 2, "{direct:?}");
    assert!(direct[1].contains("Active warnings: 2"), "{direct:?}");
    assert!(direct[1].contains("spam"), "{direct:?}");
}

// ---- moderator, bot and locale ----

#[tokio::test]
async fn the_bot_is_the_moderator_of_what_it_does_when_its_id_is_known() {
    let harness = harness();
    assert_eq!(harness.bot_id(), Some(Id::new(BOT)));
    harness.invoke(kick_of(MEMBER, Some("raid"))).await.unwrap();
    let replies = harness.invoke(kick_of(FREE, None)).await.unwrap();
    assert!(text(&replies).contains("case #2"), "{replies:?}");
    harness.invoke(target("bot_kick", MEMBER)).await.unwrap();
    let case = harness.cases().get(3).await.unwrap().unwrap();
    assert_eq!(case.moderator, Id::new(BOT));
    assert_eq!(case.kind.as_str(), "kick");
    let kicked = harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::Kick { reason, .. } => Some(reason.as_str().to_owned()),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(kicked, "Automod");
}

#[tokio::test]
async fn the_bot_has_no_moderator_when_its_id_is_unknown() {
    let harness = harness();
    harness.set_bot_id(None);
    let result = harness.invoke(target("bot_kick", MEMBER)).await;
    assert!(result.is_err(), "{result:?}");
    assert!(sequence(&harness).is_empty());
    assert!(cases_of(&harness, MEMBER).await.is_empty());
}

#[tokio::test]
async fn the_bot_id_can_be_overridden_for_a_test() {
    let harness = harness();
    harness.set_bot_id(Some(Id::new(42)));
    harness.invoke(target("bot_kick", MEMBER)).await.unwrap();
    assert_eq!(cases_of(&harness, MEMBER).await[0].moderator, Id::new(42));
}

#[tokio::test]
async fn the_notes_default_to_the_server_language_and_the_log_speaks_it_too() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness.set_log_channel(50);
    harness
        .api()
        .fail_nth(1, DiscordError::DirectMessagesClosed);
    let replies = harness
        .invoke(kick_of(MEMBER, Some("荒らし")))
        .await
        .unwrap();
    assert!(
        text(&replies).contains("ダイレクトメッセージは届きませんでした"),
        "the reply language is the server's by default: {replies:?}"
    );
    let title = harness.sent_embeds()[0].title.clone().unwrap_or_default();
    assert!(
        title.contains("キック"),
        "the log speaks the server language: {title}"
    );
}

#[tokio::test]
async fn a_reason_is_escaped_in_the_direct_message_but_kept_as_typed_in_the_case() {
    let harness = harness();
    harness
        .invoke(kick_of(MEMBER, Some("*loud* @everyone")))
        .await
        .unwrap();
    let escaped = closure_sdk::embed::escape("*loud* @everyone");
    assert!(
        dm_contents(&harness)[0].contains(&escaped),
        "{:?}",
        dm_contents(&harness)
    );
    let case = harness.cases().get(1).await.unwrap().unwrap();
    assert_eq!(case.reason.as_deref(), Some("*loud* @everyone"));
}

#[tokio::test]
async fn every_action_is_posted_to_the_log_under_its_kind() {
    let harness = harness();
    harness.set_log_channel(50);
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(target("untimeout", MEMBER)).await.unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(kick_of(MEMBER, Some("rude"))).await.unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban_of(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness.invoke(target("unban", MEMBER)).await.unwrap();

    let titles: Vec<String> = harness
        .sent_embeds()
        .iter()
        .map(|embed| embed.title.clone().unwrap_or_default())
        .collect();
    assert_eq!(
        titles,
        [
            "Case #1 · Timeout",
            "Case #2 · Timeout removed",
            "Case #3 · Kick",
            "Case #4 · Ban",
            "Case #5 · Unban",
        ]
    );
}

#[tokio::test]
async fn the_direct_message_is_in_the_server_language() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness
        .invoke(kick_of(MEMBER, Some("荒らし")))
        .await
        .unwrap();
    let messages = dm_contents(&harness);
    assert!(
        messages[0].contains("サーバーからキックされました"),
        "{messages:?}"
    );
    assert!(messages[0].contains("荒らし"), "{messages:?}");
}

#[tokio::test]
async fn a_punisher_can_answer_in_another_language_than_the_server() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness
        .api()
        .fail_nth(1, DiscordError::DirectMessagesClosed);
    let replies = harness
        .invoke(target("english_kick", MEMBER))
        .await
        .unwrap();
    assert!(
        text(&replies).contains("could not be delivered"),
        "{replies:?}"
    );
}

// ---- the jobs, as rows scheduled before the move keep them ----

#[tokio::test]
async fn a_row_of_the_old_unban_job_lifts_the_ban_when_due() {
    let harness = harness();
    harness
        .scheduler()
        .job("unban", &json!({ "user": MEMBER, "case": 1 }))
        .unwrap()
        .key(&format!("unban:{MEMBER}"))
        .after(Duration::from_secs(60))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(60));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["unban"]);
}

#[tokio::test]
async fn a_row_of_the_old_untimeout_job_takes_its_role_off_when_due() {
    let harness = harness();
    harness
        .scheduler()
        .job(
            "untimeout",
            &json!({ "user": MEMBER, "case": 1, "role": 30 }),
        )
        .unwrap()
        .key(&format!("untimeout:{MEMBER}"))
        .after(Duration::from_secs(60))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(60));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert!(role_changed(&harness, 30, false));
}

#[tokio::test]
async fn a_row_of_the_old_voice_unmute_job_lifts_the_mute_when_due() {
    let harness = harness();
    harness
        .scheduler()
        .job(
            "vc_unmute",
            &json!({ "user": MEMBER, "case": 1, "via": "discord", "role": null, "due": 0 }),
        )
        .unwrap()
        .key(&format!("vcunmute:{MEMBER}"))
        .after(Duration::from_secs(60))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(60));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(sequence(&harness), ["vcunmute"]);
}

fn settings_of(config: serde_json::Value) -> Settings {
    serde_json::from_value::<Setup>(config).unwrap().settings()
}

#[test]
fn the_settings_default_to_a_direct_message_and_discord() {
    assert_eq!(
        settings_of(json!({})),
        Settings {
            dm: true,
            timeout: Method::Discord,
            timeout_role: None,
            voice: Method::Discord,
            voice_role: None,
            strip: Strip::default(),
        }
    );
    assert_eq!(Method::default(), Method::Discord);
}

#[test]
fn an_unreadable_role_setting_is_no_role() {
    let settings = settings_of(
        json!({ "timeout": "role", "timeout_role": "0", "voice": "role", "voice_role": "x" }),
    );
    assert_eq!(settings.timeout, Method::Role);
    assert_eq!(settings.timeout_role, None);
    assert_eq!(settings.voice_role, None);
}

#[test]
fn the_moderator_of_an_interaction_is_its_user() {
    let user = closure_sdk::UserInfo {
        id: Id::new(MEMBER),
        name: "mod".to_owned(),
        bot: false,
    };
    assert_eq!(
        Moderator::of(&user),
        Moderator {
            id: Id::new(MEMBER),
            name: "mod".to_owned(),
        }
    );
}

// ---- undoing: a lift revokes the case it names ----

/// An undo command for `user` that names `case`.
fn undo_of(name: &str, user: u64, case: i64) -> Invocation {
    target(name, user).option("case", CommandOptionValue::Integer(case))
}

fn revoked_reason(case: &Case) -> Option<String> {
    case.revoked
        .as_ref()
        .and_then(|revoked| revoked.reason.clone())
}

#[tokio::test]
async fn undoing_a_timeout_revokes_only_the_case_it_names() {
    let harness = harness();
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();

    let replies = harness
        .invoke(undo_of("undo_untimeout", MEMBER, 2))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #3"), "{replies:?}");
    assert_eq!(sequence(&harness).last(), Some(&"untimeout"));

    let first = harness.cases().get(1).await.unwrap().unwrap();
    assert!(first.revoked.is_none(), "{first:?}");
    let second = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(
        revoked_reason(&second).as_deref(),
        Some("removed by case #3")
    );
}

#[tokio::test]
async fn without_undoing_a_lift_revokes_the_newest_timeout_that_stands() {
    let harness = harness();
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.invoke(target("untimeout", MEMBER)).await.unwrap();

    let first = harness.cases().get(1).await.unwrap().unwrap();
    assert!(first.revoked.is_none(), "{first:?}");
    let second = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(
        revoked_reason(&second).as_deref(),
        Some("removed by case #3")
    );
}

#[tokio::test]
async fn undoing_an_unban_revokes_only_the_ban_it_names() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban_of(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();

    harness
        .invoke(undo_of("undo_unban", MEMBER, 2))
        .await
        .unwrap();
    assert_eq!(sequence(&harness).last(), Some(&"unban"));

    let first = harness.cases().get(1).await.unwrap().unwrap();
    assert!(first.revoked.is_none(), "{first:?}");
    let second = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(
        revoked_reason(&second).as_deref(),
        Some("removed by case #3")
    );
}

#[tokio::test]
async fn without_undoing_an_unban_revokes_the_newest_ban_that_stands() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban_of(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    harness.invoke(target("unban", MEMBER)).await.unwrap();

    let first = harness.cases().get(1).await.unwrap().unwrap();
    assert!(first.revoked.is_none(), "{first:?}");
    let second = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(
        revoked_reason(&second).as_deref(),
        Some("unbanned by case #3")
    );
}

#[tokio::test]
async fn undoing_a_voice_unmute_revokes_only_the_mute_it_names() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();

    harness
        .invoke(undo_of("undo_vcunmute", MEMBER, 2))
        .await
        .unwrap();
    assert_eq!(sequence(&harness).last(), Some(&"vcunmute"));

    let first = harness.cases().get(1).await.unwrap().unwrap();
    assert!(first.revoked.is_none(), "{first:?}");
    let second = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(
        revoked_reason(&second).as_deref(),
        Some("removed by case #3")
    );
}

#[tokio::test]
async fn without_undoing_a_voice_unmute_revokes_the_newest_mute_that_stands() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.invoke(target("vcunmute", MEMBER)).await.unwrap();

    let first = harness.cases().get(1).await.unwrap().unwrap();
    assert!(first.revoked.is_none(), "{first:?}");
    let second = harness.cases().get(2).await.unwrap().unwrap();
    assert_eq!(
        revoked_reason(&second).as_deref(),
        Some("removed by case #3")
    );
}

// ---- a lift does not end a newer punishment that still stands ----

/// A case another module writes against `user` through `cases()`, as the module would.
async fn other_module_case(
    harness: &Harness,
    kind: &str,
    user: u64,
    expires_at: Option<i64>,
) -> Case {
    harness
        .cases()
        .create(NewCase {
            kind: CaseKind::new(kind).unwrap(),
            target: Id::new(user),
            moderator: Id::new(BOT),
            reason: None,
            expires_at,
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_role_timeout_lift_waits_while_a_newer_timeout_of_another_module_stands() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "10m"))
        .await
        .unwrap();
    let first = harness.cases().get(1).await.unwrap().unwrap();
    other_module_case(&harness, "timeout", MEMBER, Some(first.created_at + 3_600)).await;

    harness.advance(Duration::from_secs(600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert!(
        !role_changed(&harness, 30, false),
        "the newer timeout still holds the role"
    );
}

#[tokio::test]
async fn a_role_timeout_lift_goes_on_when_only_its_own_timeout_stands() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "10m"))
        .await
        .unwrap();

    harness.advance(Duration::from_secs(600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert!(role_changed(&harness, 30, false));
}

#[tokio::test]
async fn a_role_timeout_lift_goes_on_when_the_other_timeout_has_run_out() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "10m"))
        .await
        .unwrap();
    let first = harness.cases().get(1).await.unwrap().unwrap();
    other_module_case(&harness, "timeout", MEMBER, Some(first.created_at + 300)).await;

    harness.advance(Duration::from_secs(600));
    harness.run_due_jobs().await;
    assert!(role_changed(&harness, 30, false));
}

#[tokio::test]
async fn a_voice_role_lift_waits_while_a_newer_mute_of_another_module_stands() {
    let harness = harness();
    harness.set_config(json!({ "voice": "role", "voice_role": "31" }));
    harness
        .invoke(mute_of(MEMBER, Some("10m"), None))
        .await
        .unwrap();
    other_module_case(&harness, "vcmute", MEMBER, None).await;

    harness.advance(Duration::from_secs(600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert!(!role_changed(&harness, 31, false));
}

#[tokio::test]
async fn a_voice_role_lift_goes_on_when_only_its_own_mute_stands() {
    let harness = harness();
    harness.set_config(json!({ "voice": "role", "voice_role": "31" }));
    harness
        .invoke(mute_of(MEMBER, Some("10m"), None))
        .await
        .unwrap();

    harness.advance(Duration::from_secs(600));
    harness.run_due_jobs().await;
    assert!(role_changed(&harness, 31, false));
}

#[tokio::test]
async fn a_voice_lift_by_discord_waits_while_a_newer_mute_of_another_module_stands() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("10m"), None))
        .await
        .unwrap();
    let first = harness.cases().get(1).await.unwrap().unwrap();
    other_module_case(&harness, "vcmute", MEMBER, Some(first.created_at + 3_600)).await;

    harness.advance(Duration::from_secs(600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert!(
        !sequence(&harness).contains(&"vcunmute"),
        "{:?}",
        sequence(&harness)
    );
}

#[tokio::test]
async fn a_voice_lift_by_discord_goes_on_when_only_its_own_mute_stands() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("10m"), None))
        .await
        .unwrap();

    harness.advance(Duration::from_secs(600));
    harness.run_due_jobs().await;
    assert_eq!(sequence(&harness).last(), Some(&"vcunmute"));
}

#[tokio::test]
async fn a_timed_ban_lift_waits_while_a_permanent_ban_of_another_module_stands() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    other_module_case(&harness, "ban", MEMBER, None).await;

    harness.advance(Duration::from_secs(DAY));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert!(
        !sequence(&harness).contains(&"unban"),
        "{:?}",
        sequence(&harness)
    );
}

#[tokio::test]
async fn a_timed_ban_lift_goes_on_when_only_its_own_ban_stands() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();

    harness.advance(Duration::from_secs(DAY));
    harness.run_due_jobs().await;
    assert_eq!(sequence(&harness).last(), Some(&"unban"));
}

// ---- a role lift follows the newer punishment that still holds the member ----

#[tokio::test]
async fn a_role_timeout_lift_moves_to_the_end_of_a_newer_timeout_that_stands() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "10m"))
        .await
        .unwrap();
    let first = harness.cases().get(1).await.unwrap().unwrap();
    other_module_case(&harness, "timeout", MEMBER, Some(first.created_at + 3_600)).await;

    harness.advance(Duration::from_secs(600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert!(
        !role_changed(&harness, 30, false),
        "the newer timeout holds the role"
    );
    assert_eq!(
        harness.jobs(),
        1,
        "the lift is set for the newer timeout's end"
    );

    harness.advance(Duration::from_secs(3_000));
    harness.run_due_jobs().await;
    assert!(role_changed(&harness, 30, false));
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_role_timeout_lift_does_nothing_while_an_open_ended_timeout_stands() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "10m"))
        .await
        .unwrap();
    other_module_case(&harness, "timeout", MEMBER, None).await;

    harness.advance(Duration::from_secs(600));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert_eq!(harness.jobs(), 0, "nothing is set again");
    harness.advance(Duration::from_secs(10 * DAY));
    harness.run_due_jobs().await;
    assert!(
        !role_changed(&harness, 30, false),
        "the open-ended timeout keeps the role"
    );
}

#[tokio::test]
async fn a_role_timeout_lift_follows_the_latest_end_as_more_timeouts_follow() {
    let harness = harness();
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30" }));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "10m"))
        .await
        .unwrap();
    let first = harness.cases().get(1).await.unwrap().unwrap();
    other_module_case(&harness, "timeout", MEMBER, Some(first.created_at + 3_600)).await;

    harness.advance(Duration::from_secs(600));
    harness.run_due_jobs().await;
    assert_eq!(harness.jobs(), 1, "set for 3600");

    // A later one is made before the lift is due, and ends after the first.
    harness.advance(Duration::from_secs(1_200));
    other_module_case(&harness, "timeout", MEMBER, Some(first.created_at + 5_400)).await;
    harness.advance(Duration::from_secs(1_800));
    harness.run_due_jobs().await;
    assert!(
        !role_changed(&harness, 30, false),
        "the later timeout holds the role"
    );
    assert_eq!(harness.jobs(), 1, "set again for 5400");

    harness.advance(Duration::from_secs(1_800));
    harness.run_due_jobs().await;
    assert!(role_changed(&harness, 30, false));
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_role_lift_moves_to_the_end_of_a_newer_discord_mute_that_stands() {
    let harness = harness();
    harness.set_config(json!({ "voice": "role", "voice_role": "31" }));
    harness
        .invoke(mute_of(MEMBER, Some("10m"), None))
        .await
        .unwrap();
    let first = harness.cases().get(1).await.unwrap().unwrap();
    other_module_case(&harness, "vcmute", MEMBER, Some(first.created_at + 3_600)).await;

    harness.advance(Duration::from_secs(600));
    harness.run_due_jobs().await;
    assert!(
        !role_changed(&harness, 31, false),
        "the newer mute holds the role"
    );
    assert_eq!(
        harness.jobs(),
        1,
        "the lift is set for the newer mute's end"
    );

    harness.advance(Duration::from_secs(3_000));
    harness.run_due_jobs().await;
    assert!(role_changed(&harness, 31, false));
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_voice_lift_by_discord_moves_to_the_end_of_a_newer_role_mute_that_stands() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("10m"), None))
        .await
        .unwrap();
    let first = harness.cases().get(1).await.unwrap().unwrap();
    other_module_case(&harness, "vcmute", MEMBER, Some(first.created_at + 3_600)).await;

    harness.advance(Duration::from_secs(600));
    harness.run_due_jobs().await;
    assert!(
        !sequence(&harness).contains(&"vcunmute"),
        "{:?}",
        sequence(&harness)
    );
    assert_eq!(
        harness.jobs(),
        1,
        "the lift is set for the newer mute's end"
    );

    harness.advance(Duration::from_secs(3_000));
    harness.run_due_jobs().await;
    assert_eq!(sequence(&harness).last(), Some(&"vcunmute"));
    assert_eq!(harness.jobs(), 0);
}

// ---- undoing an older case while a newer one stands ----

#[tokio::test]
async fn undoing_an_older_timeout_lifts_nothing_while_a_newer_one_stands() {
    let harness = harness();
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await
        .unwrap();
    let before = sequence(&harness);

    let replies = harness
        .invoke(undo_of("undo_untimeout", MEMBER, 1))
        .await
        .unwrap();
    assert!(text(&replies).contains("case #1"), "{replies:?}");
    assert_eq!(sequence(&harness), before, "Discord is not called");
    assert_eq!(
        cases_of(&harness, MEMBER).await.len(),
        2,
        "no lift recorded"
    );
    let first = harness.cases().get(1).await.unwrap().unwrap();
    assert!(first.revoked.is_none(), "the caller revokes it: {first:?}");
}

#[tokio::test]
async fn undoing_an_older_ban_lifts_nothing_while_a_newer_one_stands() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(ban_of(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    let before = sequence(&harness);

    harness
        .invoke(undo_of("undo_unban", MEMBER, 1))
        .await
        .unwrap();
    assert_eq!(sequence(&harness), before, "Discord is not called");
    assert_eq!(
        cases_of(&harness, MEMBER).await.len(),
        2,
        "no lift recorded"
    );
}

#[tokio::test]
async fn undoing_an_older_voice_mute_lifts_nothing_while_a_newer_one_stands() {
    let harness = harness();
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    harness.advance(Duration::from_secs(3));
    harness
        .invoke(mute_of(MEMBER, Some("1h"), Some("noisy")))
        .await
        .unwrap();
    let before = sequence(&harness);

    harness
        .invoke(undo_of("undo_vcunmute", MEMBER, 1))
        .await
        .unwrap();
    assert_eq!(sequence(&harness), before, "Discord is not called");
    assert_eq!(
        cases_of(&harness, MEMBER).await.len(),
        2,
        "no lift recorded"
    );
}

// ---- a failure is reported, not hidden: the lift, the recording and the roles ----

#[tokio::test]
async fn an_undo_of_a_case_already_revoked_is_reported_not_hidden() {
    let harness = harness();
    harness
        .invoke(ban_of(MEMBER, Some("raid"), None, None))
        .await
        .unwrap();
    harness.cases().revoke(1, Id::new(BOT), None).await.unwrap();

    let result = harness.invoke(undo_of("undo_unban", MEMBER, 1)).await;
    assert!(result.is_err(), "the failed revoke is reported: {result:?}");
}

#[tokio::test]
async fn a_ban_that_cannot_be_recorded_says_the_ban_went_through() {
    let harness = harness();
    // A reason longer than a case keeps: the recording fails after Discord has banned.
    let reason = "a".repeat(513);
    let result = harness
        .invoke(with_reason(
            target("ban_raw", MEMBER),
            Some(reason.as_str()),
        ))
        .await;
    let message = match result {
        Err(error) => format!("{error:?}"),
        Ok(replies) => panic!("the failure is reported, not answered: {replies:?}"),
    };
    assert!(message.contains("went through"), "{message}");
    assert_eq!(sequence(&harness).last(), Some(&"ban"), "Discord banned");
    assert!(cases_of(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_role_timeout_whose_roles_cannot_be_read_still_takes_the_timeout() {
    let harness = harness();
    harness
        .api()
        .set_roles(Id::new(1), vec![guild_role(700, 1)]);
    harness
        .api()
        .set_member(Id::new(1), member_with_roles(MEMBER, &[700]));
    harness.set_config(json!({ "timeout": "role", "timeout_role": "30", "strip_remove": true }));
    // The role is given first; the member read that takes the other roles off is the second call.
    harness
        .api()
        .fail_nth(2, DiscordError::Forbidden("missing permission".to_owned()));

    let result = harness
        .invoke(with_duration(target("timeout", MEMBER), "1h"))
        .await;
    assert!(
        role_changed(&harness, 30, true),
        "the timeout role was given"
    );
    // Not refused: the same as a log that cannot take a case. The roles are simply left on.
    assert!(result.is_ok(), "{result:?}");
    assert!(!role_changed(&harness, 700, false), "no role was taken off");
}

// ---- a timed ban lift looks past the newest cases for an older ban that stands ----

#[tokio::test]
async fn a_timed_ban_lift_finds_an_older_ban_behind_a_hundred_other_cases() {
    let harness = harness();
    other_module_case(&harness, "ban", MEMBER, None).await;
    harness
        .invoke(ban_of(MEMBER, Some("raid"), Some("1d"), None))
        .await
        .unwrap();
    for _ in 0..120 {
        other_module_case(&harness, "warn", MEMBER, None).await;
    }

    harness.advance(Duration::from_secs(DAY));
    let ended = harness.run_due_jobs().await;
    assert!(ended.len() == 1 && ended[0].is_ok(), "{ended:?}");
    assert!(
        !sequence(&harness).contains(&"unban"),
        "the older ban still stands: {:?}",
        sequence(&harness)
    );
}
