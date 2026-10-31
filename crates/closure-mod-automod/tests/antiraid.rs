//! Antiraid: the levels and what each one changes, the quarantine of joining members, the commands,
//! and the trigger and action that let a rule raise the level.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a broken setup should panic at once"
)]
#![allow(clippy::too_many_lines, reason = "a scenario reads best as one block")]
#![allow(
    clippy::needless_pass_by_value,
    reason = "helpers take the JSON they embed by value, which keeps the rules readable at the call site"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use closure_config::{ConfigCompiler, parse_overrides};
use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::{CommandOptionValue, MemberInfo, UserInfo};
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use closure_sdk::Event;
use closure_sdk::testing::{Autocomplete, Harness, Invocation, Reply};
use common::*;
use serde_json::{Value, json};
use tokio::time::timeout;
use twilight_model::channel::Channel;
use twilight_model::guild::Role;
use twilight_model::id::Id;

const LEVELS: [&str; 3] = ["calm", "low", "high"];

/// A harness with antiraid set up as `antiraid` says, and the guild's text channels declared.
fn raid(antiraid: Value) -> Harness {
    let harness = configured(json!({ "antiraid": antiraid }));
    text_channels(&harness);
    harness
}

fn text_channel(id: u64, slowmode: u16) -> Channel {
    serde_json::from_value(json!({
        "id": id.to_string(), "type": 0, "guild_id": GUILD.to_string(),
        "name": format!("channel-{id}"), "permission_overwrites": [],
        "rate_limit_per_user": slowmode
    }))
    .unwrap()
}

/// Channel 5 has a slowmode of 3 seconds, the others none.
fn text_channels(harness: &Harness) {
    harness.api().set_channels(
        Id::new(GUILD),
        vec![
            text_channel(CHANNEL, 3),
            text_channel(OTHER_CHANNEL, 0),
            text_channel(NOTIFY, 0),
        ],
    );
}

fn role(id: u64, name: &str) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": name, "color": 0, "hoist": false, "managed": false,
        "mentionable": false, "permissions": "0", "position": 1, "flags": 0,
        "icon": null, "unicode_emoji": null, "tags": null,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    }))
    .unwrap()
}

/// The `/automod antiraid <sub>` command, run by a moderator.
fn sub(path: &str) -> Invocation {
    Invocation::new("automod")
        .subcommand(path)
        .roles([MODERATOR_ROLE])
}

/// The same, run by an administrator.
fn admin(path: &str) -> Invocation {
    sub(path).administrator()
}

fn set_invocation(level: &str) -> Invocation {
    admin("antiraid set").option("level", CommandOptionValue::String(level.to_owned()))
}

async fn set(harness: &Harness, level: &str) {
    harness.invoke(set_invocation(level)).await.unwrap();
}

async fn clear(harness: &Harness) {
    harness.invoke(admin("antiraid clear")).await.unwrap();
}

/// The text of the replies: their content and the descriptions of their embeds.
fn text_of(replies: &[Reply]) -> String {
    replies
        .iter()
        .map(|reply| {
            let embeds: Vec<String> = reply
                .embeds
                .iter()
                .filter_map(|embed| embed.description.clone())
                .collect();
            format!("{}\n{}", reply.content, embeds.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn view(harness: &Harness) -> String {
    text_of(&harness.invoke(sub("antiraid view")).await.unwrap())
}

/// The slowmode each channel was asked to have, in the order the module asked.
fn channel_edits(harness: &Harness) -> Vec<(u64, Option<u16>)> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::EditChannel {
                channel_id, patch, ..
            } => Some((channel_id.get(), patch.slowmode_seconds)),
            _ => None,
        })
        .collect()
}

/// The invites flag each guild edit asked for.
fn guild_edits(harness: &Harness) -> Vec<Option<bool>> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::EditGuild { patch, .. } => Some(patch.invites_disabled),
            _ => None,
        })
        .collect()
}

/// The role changes asked for, as (added, member, role).
fn role_changes(harness: &Harness) -> Vec<(bool, u64, u64)> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::AddRole {
                user_id, role_id, ..
            } => Some((true, user_id.get(), role_id.get())),
            Action::RemoveRole {
                user_id, role_id, ..
            } => Some((false, user_id.get(), role_id.get())),
            _ => None,
        })
        .collect()
}

fn texts_of_channel(harness: &Harness, channel: u64, needle: &str) -> usize {
    sent_texts(harness)
        .iter()
        .filter(|(sent_to, text)| *sent_to == channel && text.contains(needle))
        .count()
}

async fn join_bot(harness: &Harness, user: u64) {
    harness
        .emit(Event::MemberJoined(MemberInfo {
            guild_id: Id::new(GUILD),
            user: UserInfo {
                id: Id::new(user),
                name: "bot".into(),
                bot: true,
            },
            roles: Vec::new(),
            nick: None,
            joined_at: Some(unix_now()),
            timed_out_until: None,
        }))
        .await
        .unwrap();
}

fn words(word: &str) -> Value {
    json!({ "type": "words", "words": [word] })
}

// ---- configuration ----

#[tokio::test]
async fn an_invalid_antiraid_section_disables_antiraid_and_not_the_rules() {
    let harness = configured(json!({
        "antiraid": { "levels": ["calm", "calm"], "effects": { "high": { "pause_invites": true } } },
        "rules": rules(&[("spoilers", rule(json!([words("spoiler")]), delete()))])
    }));
    text_channels(&harness);
    say(&harness, MEMBER, "spoiler").await;
    assert_eq!(deletions(&harness), 1, "the rule still runs");
    set(&harness, "high").await;
    assert!(
        guild_edits(&harness).is_empty(),
        "antiraid does nothing while its section is invalid"
    );
}

#[tokio::test]
async fn an_effect_for_a_level_that_does_not_exist_is_invalid() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "bogus": { "pause_invites": true } }
    }));
    set(&harness, "high").await;
    assert!(guild_edits(&harness).is_empty());
    assert!(channel_edits(&harness).is_empty());
}

// ---- what a level changes ----

#[tokio::test]
async fn a_level_sets_the_slowmode_of_every_text_channel_and_clear_restores_the_originals() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "low": { "slowmode": { "seconds": 10 } } }
    }));
    set(&harness, "low").await;
    let edits = channel_edits(&harness);
    for channel in [CHANNEL, OTHER_CHANNEL, NOTIFY] {
        assert!(
            edits.contains(&(channel, Some(10))),
            "channel {channel} gets the slowmode: {edits:?}"
        );
    }

    clear(&harness).await;
    let restored = channel_edits(&harness)[edits.len()..].to_vec();
    assert!(
        restored.contains(&(CHANNEL, Some(3))),
        "the original slowmode of channel 5 comes back: {restored:?}"
    );
    assert!(restored.contains(&(OTHER_CHANNEL, Some(0))));
    assert!(view(&harness).await.contains("calm"));
}

#[tokio::test]
async fn an_explicit_channel_list_limits_the_slowmode_to_those_channels() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "channels": [CHANNEL.to_string()], "seconds": 30 } } }
    }));
    set(&harness, "high").await;
    assert_eq!(channel_edits(&harness), vec![(CHANNEL, Some(30))]);
}

#[tokio::test]
async fn a_failing_effect_is_counted_and_the_other_channels_still_change() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    }));
    // The list of the channels is the first call; the first slowmode edit is the second.
    harness
        .api()
        .fail_nth(2, DiscordError::Forbidden("no".into()));
    let replies = harness.invoke(set_invocation("high")).await;
    assert!(replies.is_ok(), "the command itself succeeds");
    let edited: Vec<u64> = channel_edits(&harness).iter().map(|(id, _)| *id).collect();
    assert!(edited.contains(&OTHER_CHANNEL) && edited.contains(&NOTIFY));
    assert!(
        !edited.contains(&CHANNEL),
        "the failed edit is not recorded"
    );
}

#[tokio::test]
async fn a_level_that_pauses_invites_pauses_them_and_leaving_it_resumes_them_once() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "pause_invites": true } }
    }));
    set(&harness, "high").await;
    assert_eq!(guild_edits(&harness), vec![Some(true)]);
    set(&harness, "low").await;
    assert_eq!(guild_edits(&harness), vec![Some(true), Some(false)]);
    set(&harness, "calm").await;
    assert_eq!(
        guild_edits(&harness),
        vec![Some(true), Some(false)],
        "invites that were not paused by antiraid are left alone"
    );
}

#[tokio::test]
async fn a_change_is_announced_in_the_log_and_in_the_notify_channel_of_the_level() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 5 }, "notify_channel": NOTIFY.to_string() } }
    }));
    set(&harness, "high").await;
    assert_eq!(
        texts_of_channel(&harness, LOG, "high"),
        1,
        "the log channel is told"
    );
    assert_eq!(
        texts_of_channel(&harness, NOTIFY, "high"),
        1,
        "the notify channel is told"
    );
}

// ---- the commands ----

#[tokio::test]
async fn setting_and_clearing_need_an_administrator_and_viewing_needs_a_moderator() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "pause_invites": true } }
    }));
    // A moderator may look, but not change the level.
    let _ = harness
        .invoke(sub("antiraid set").option("level", CommandOptionValue::String("high".into())))
        .await;
    assert!(
        guild_edits(&harness).is_empty(),
        "a moderator cannot set the level"
    );
    assert!(view(&harness).await.contains("calm"));
}

#[tokio::test]
async fn the_level_names_are_offered_while_typing() {
    let harness = raid(json!({ "levels": LEVELS }));
    let choices = harness
        .autocomplete(
            Autocomplete::new("automod", "level", "hi")
                .subcommand("antiraid set")
                .administrator(),
        )
        .await
        .unwrap();
    assert!(choices.iter().any(|choice| choice.name == "high"));
}

#[tokio::test]
async fn the_status_shows_the_current_level() {
    let harness = raid(json!({ "levels": LEVELS }));
    set(&harness, "low").await;
    let replies = harness.invoke(sub("status")).await.unwrap();
    assert!(
        text_of(&replies).contains("low"),
        "the status names the level"
    );
}

// ---- expiry ----

#[tokio::test]
async fn an_expiry_returns_to_the_first_level_when_its_time_is_up() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    }));
    harness
        .invoke(set_invocation("high").option("duration", CommandOptionValue::String("1m".into())))
        .await
        .unwrap();
    harness.advance(std::time::Duration::from_secs(120));
    let results = harness.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok));
    assert!(view(&harness).await.contains("calm"));
}

#[tokio::test]
async fn an_expiry_does_not_undo_a_later_change() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    }));
    harness
        .invoke(set_invocation("high").option("duration", CommandOptionValue::String("1m".into())))
        .await
        .unwrap();
    set(&harness, "low").await;
    harness.advance(std::time::Duration::from_secs(120));
    harness.run_due_jobs().await;
    assert!(
        view(&harness).await.contains("low"),
        "the later level stays"
    );
}

// ---- quarantine ----

#[tokio::test]
async fn a_member_who_joins_at_a_quarantining_level_gets_the_auto_created_role() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": {} } }
    }));
    set(&harness, "high").await;
    join(&harness, MEMBER).await;
    assert!(
        role_changes(&harness)
            .iter()
            .any(|(added, user, role)| *added && *user == MEMBER && *role != 0),
        "the joining member gets the quarantine role: {:?}",
        role_changes(&harness)
    );
}

#[tokio::test]
async fn a_member_who_joins_at_a_calm_level_is_not_quarantined() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": {} } }
    }));
    join(&harness, MEMBER).await;
    assert!(role_changes(&harness).is_empty());
}

#[tokio::test]
async fn bots_are_not_quarantined() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": {} } }
    }));
    set(&harness, "high").await;
    join_bot(&harness, MEMBER).await;
    assert!(role_changes(&harness).is_empty());
}

#[tokio::test]
async fn an_explicit_quarantine_role_is_used_when_the_guild_has_it() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": { "role": ROLE.to_string() } } }
    }));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(ROLE, "Quarantine")]);
    set(&harness, "high").await;
    join(&harness, MEMBER).await;
    assert!(role_changes(&harness).contains(&(true, MEMBER, ROLE)));
}

#[tokio::test]
async fn release_removes_the_quarantine_of_one_member_or_of_all() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": { "role": ROLE.to_string() } } }
    }));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(ROLE, "Quarantine")]);
    set(&harness, "high").await;
    join(&harness, MEMBER).await;
    join(&harness, OTHER_MEMBER).await;

    harness
        .invoke(sub("antiraid release").option("user", CommandOptionValue::User(Id::new(MEMBER))))
        .await
        .unwrap();
    assert!(role_changes(&harness).contains(&(false, MEMBER, ROLE)));
    assert!(!role_changes(&harness).contains(&(false, OTHER_MEMBER, ROLE)));

    harness.invoke(sub("antiraid release-all")).await.unwrap();
    assert!(role_changes(&harness).contains(&(false, OTHER_MEMBER, ROLE)));
}

#[tokio::test]
async fn a_member_who_leaves_is_forgotten_by_the_quarantine() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": { "role": ROLE.to_string() } } }
    }));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(ROLE, "Quarantine")]);
    set(&harness, "high").await;
    join(&harness, MEMBER).await;
    leave(&harness, MEMBER).await;
    harness.invoke(sub("antiraid release-all")).await.unwrap();
    assert!(
        !role_changes(&harness).contains(&(false, MEMBER, ROLE)),
        "a member who left is not released again"
    );
}

// ---- the trigger and the action ----

#[tokio::test]
async fn a_trigger_fires_when_the_level_changes_to_one_it_names() {
    let harness = raid(json!({ "levels": LEVELS }));
    harness.set_config(json!({
        "antiraid": { "levels": LEVELS },
        "rules": rules(&[(
            "watch",
            rule(
                json!([{ "type": "antiraid_level", "levels": ["high"] }]),
                json!([{ "do": "alert", "channel": NOTIFY.to_string(), "text": "raised" }]),
            ),
        )])
    }));
    text_channels(&harness);
    set(&harness, "low").await;
    assert_eq!(texts_of_channel(&harness, NOTIFY, "raised"), 0);
    set(&harness, "high").await;
    assert_eq!(texts_of_channel(&harness, NOTIFY, "raised"), 1);
}

#[tokio::test]
async fn a_trigger_with_from_fires_only_from_the_named_level() {
    let harness = raid(json!({ "levels": LEVELS }));
    harness.set_config(json!({
        "antiraid": { "levels": LEVELS },
        "rules": rules(&[(
            "watch",
            rule(
                json!([{ "type": "antiraid_level", "from": ["calm"] }]),
                json!([{ "do": "alert", "channel": NOTIFY.to_string(), "text": "left calm" }]),
            ),
        )])
    }));
    text_channels(&harness);
    set(&harness, "low").await;
    set(&harness, "high").await;
    assert_eq!(texts_of_channel(&harness, NOTIFY, "left calm"), 1);
}

#[tokio::test]
async fn a_chain_of_level_changes_is_cut_at_its_depth() {
    let harness = raid(json!({ "levels": LEVELS }));
    harness.set_config(json!({
        "antiraid": { "levels": LEVELS },
        "rules": rules(&[
            (
                "up",
                rule(
                    json!([{ "type": "antiraid_level", "levels": ["low"] }]),
                    json!([{ "do": "set_antiraid_level", "level": "high" }]),
                ),
            ),
            (
                "down",
                rule(
                    json!([{ "type": "antiraid_level", "levels": ["high"] }]),
                    json!([{ "do": "set_antiraid_level", "level": "low" }]),
                ),
            ),
        ])
    }));
    text_channels(&harness);
    let result = harness.invoke(set_invocation("low")).await;
    assert!(result.is_err(), "the chain that goes too deep is reported");
}

#[tokio::test]
async fn a_join_spam_rule_raises_the_level_and_the_slowmode_and_the_invites_follow() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 }, "pause_invites": true } }
    }));
    harness.set_config(json!({
        "antiraid": {
            "levels": LEVELS,
            "effects": { "high": { "slowmode": { "seconds": 30 }, "pause_invites": true } }
        },
        "rules": rules(&[(
            "raid",
            rule(
                json!([{ "type": "member_join_spam", "amount": 3, "within": "1m" }]),
                json!([{ "do": "set_antiraid_level", "level": "high" }]),
            ),
        )])
    }));
    text_channels(&harness);
    join(&harness, 201).await;
    join(&harness, 202).await;
    join(&harness, 203).await;
    assert!(view(&harness).await.contains("high"));
    assert!(channel_edits(&harness).contains(&(CHANNEL, Some(30))));
    assert_eq!(guild_edits(&harness), vec![Some(true)]);
}

#[tokio::test]
async fn a_dry_run_reports_the_level_change_and_changes_nothing() {
    let harness = configured(json!({
        "dry_run": true,
        "antiraid": {
            "levels": LEVELS,
            "effects": { "high": { "slowmode": { "seconds": 30 }, "pause_invites": true } }
        },
        "rules": rules(&[(
            "raid",
            rule(
                json!([{ "type": "member_join_spam", "amount": 2, "within": "1m" }]),
                json!([{ "do": "set_antiraid_level", "level": "high" }]),
            ),
        )])
    }));
    text_channels(&harness);
    join(&harness, 201).await;
    join(&harness, 202).await;
    assert!(view(&harness).await.contains("calm"));
    assert!(channel_edits(&harness).is_empty());
    assert!(guild_edits(&harness).is_empty());
}

#[tokio::test]
async fn a_dry_run_does_not_give_a_quarantine_role_to_a_joining_member() {
    let antiraid = json!({ "levels": LEVELS, "effects": { "high": { "quarantine": {} } } });
    let harness = raid(antiraid.clone());
    set(&harness, "high").await;
    harness.set_config(json!({ "dry_run": true, "antiraid": antiraid }));
    let before = harness.api().actions().len();
    join(&harness, 201).await;
    let actions = harness.api().actions();
    assert!(
        !actions[before..]
            .iter()
            .any(|action| matches!(action, Action::CreateRole { .. } | Action::AddRole { .. })),
        "a dry run gives no quarantine role to a member who joins"
    );
}

#[tokio::test]
async fn a_dry_run_level_change_creates_no_quarantine_role() {
    let antiraid = json!({ "levels": LEVELS, "effects": { "high": { "quarantine": {} } } });
    let harness = raid(antiraid.clone());
    harness.set_config(json!({ "dry_run": true, "antiraid": antiraid }));
    set(&harness, "high").await;
    let actions = harness.api().actions();
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::CreateRole { .. })),
        "a dry run sets up no role for the level"
    );
}

#[tokio::test]
async fn a_level_change_books_its_resume_before_it_makes_the_discord_changes() {
    let harness = Arc::new(raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    })));
    let store = harness.store();
    let runner = Arc::clone(&harness);
    let change = tokio::spawn(async move { set(&runner, "high").await });
    timeout(Duration::from_secs(5), async {
        while store
            .get::<Value>("antiraid:level")
            .await
            .unwrap()
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the level is written");
    change.abort();
    assert_eq!(
        harness.jobs(),
        1,
        "the resume of a level is booked as soon as the level is written"
    );
}

// ---- the configuration as the host reads it ----

#[test]
fn the_schema_accepts_an_antiraid_section_and_rejects_its_bad_shapes() {
    let registry = Arc::new(
        ModuleRegistry::new(vec![
            Arc::new(closure_mod_automod::module().unwrap()) as Arc<dyn Module>
        ])
        .unwrap(),
    );
    let compiler = ConfigCompiler::new(registry).unwrap();
    let accepted = |antiraid: Value| {
        let yaml = format!(
            "modules:\n  automod:\n    config: {}\n",
            serde_json::to_string(&json!({ "antiraid": antiraid })).unwrap()
        );
        compiler.compile(&parse_overrides(&yaml).unwrap()).is_ok()
    };
    assert!(accepted(json!({
        "levels": LEVELS,
        "effects": {
            "low": { "slowmode": { "channels": ["5"], "seconds": 10 } },
            "high": {
                "slowmode": { "seconds": 30 },
                "pause_invites": true,
                "quarantine": { "role": "77", "marker_role": "78" },
                "notify_channel": "60"
            }
        },
        "quarantine_release": false
    })));
    assert!(
        !accepted(json!({ "unknown_key": true })),
        "unknown keys are refused"
    );
    assert!(
        !accepted(json!({ "levels": ["one"] })),
        "a level list needs two levels"
    );
    assert!(
        !accepted(json!({ "levels": ["Bad Name", "ok"] })),
        "level names are plain"
    );
    assert!(!accepted(
        json!({ "effects": { "low": { "slowmode": { "seconds": 99999 } } } })
    ));
    assert!(
        !accepted(json!({ "quarantine_release": true })),
        "no automatic release"
    );
}

/// A member who joins, delivered as the gateway does, for the checks that need the error of a join.
fn joining(user: u64) -> Event {
    Event::MemberJoined(MemberInfo {
        guild_id: Id::new(GUILD),
        user: UserInfo {
            id: Id::new(user),
            name: "member".into(),
            bot: false,
        },
        roles: Vec::new(),
        nick: None,
        joined_at: Some(unix_now()),
        timed_out_until: None,
    })
}

#[tokio::test]
async fn a_failed_effect_is_applied_again_when_the_same_level_is_set() {
    // A change that stopped halfway (here: one slowmode edit failed) is not complete, and setting the
    // level it is at again must make the missing changes.
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    }));
    // The channel list is the first call; the first slowmode edit is the second.
    harness
        .api()
        .fail_nth(2, DiscordError::Forbidden("no".into()));
    set(&harness, "high").await;
    assert!(
        view(&harness).await.contains("Incomplete"),
        "the failed effect is reported"
    );

    set(&harness, "high").await;
    assert!(
        channel_edits(&harness).contains(&(CHANNEL, Some(30))),
        "setting the same level again applies the failed effect: {:?}",
        channel_edits(&harness)
    );
    assert!(!view(&harness).await.contains("Incomplete"));
}

#[tokio::test]
async fn a_failed_effect_is_applied_by_the_resume_job_without_a_command() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    }));
    harness
        .api()
        .fail_nth(2, DiscordError::Forbidden("no".into()));
    set(&harness, "high").await;

    harness.advance(std::time::Duration::from_secs(60));
    let results = harness.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok));
    assert!(
        channel_edits(&harness).contains(&(CHANNEL, Some(30))),
        "the resume job applies the failed effect: {:?}",
        channel_edits(&harness)
    );
    assert!(!view(&harness).await.contains("Incomplete"));
}

#[tokio::test]
async fn an_incomplete_level_is_shown_with_its_reason_by_the_view_and_the_status() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    }));
    harness
        .api()
        .fail_nth(2, DiscordError::Forbidden("no".into()));
    set(&harness, "high").await;
    assert!(view(&harness).await.contains("Incomplete"));
    let status = text_of(&harness.invoke(sub("status")).await.unwrap());
    assert!(
        status.contains("incomplete"),
        "the status says so: {status}"
    );
}

#[tokio::test]
async fn an_expiry_of_an_earlier_change_does_nothing_when_a_later_change_made_in_the_same_second() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    }));
    harness
        .invoke(set_invocation("high").option("duration", CommandOptionValue::String("1m".into())))
        .await
        .unwrap();
    // Both changes happen in the same second, so their start times are equal.
    harness
        .invoke(set_invocation("low").option("duration", CommandOptionValue::String("10m".into())))
        .await
        .unwrap();
    // The expiry of the first change, as if its cancellation had been lost.
    let level: Value = harness
        .store()
        .get("antiraid:level")
        .await
        .unwrap()
        .unwrap();
    let set_at = level["set_at"].as_i64().unwrap();
    harness
        .scheduler()
        .job(
            "antiraid_expire",
            &json!({ "set_at": set_at, "generation": 1 }),
        )
        .unwrap()
        .key("stale")
        .after(std::time::Duration::from_secs(60))
        .await
        .unwrap();

    harness.advance(std::time::Duration::from_secs(120));
    let results = harness.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok));
    assert!(
        view(&harness).await.contains("low"),
        "the later level stays"
    );
}

#[tokio::test]
async fn concurrent_level_changes_end_consistent_and_keep_the_original_slowmode() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": {
            "low": { "slowmode": { "seconds": 10 } },
            "high": { "slowmode": { "seconds": 30 } }
        }
    }));
    let (low, high) = tokio::join!(
        harness.invoke(set_invocation("low")),
        harness.invoke(set_invocation("high"))
    );
    low.unwrap();
    high.unwrap();

    let text = view(&harness).await;
    let (level, seconds) = if text.contains("Level: high") {
        ("high", 30)
    } else {
        ("low", 10)
    };
    assert!(text.contains(&format!("Level: {level}")), "{text}");
    let last = channel_edits(&harness)
        .into_iter()
        .rev()
        .find(|(channel, _)| *channel == CHANNEL);
    assert_eq!(
        last,
        Some((CHANNEL, Some(seconds))),
        "the channel has the slowmode of the level it is at"
    );

    clear(&harness).await;
    assert!(
        channel_edits(&harness).contains(&(CHANNEL, Some(3))),
        "the original slowmode of the channel is kept: {:?}",
        channel_edits(&harness)
    );
}

#[tokio::test]
async fn a_pause_whose_intent_cannot_be_recorded_is_not_made_and_is_retried() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "pause_invites": true } }
    }));
    // The level is stored first, so only the new invites marker can be refused.
    set(&harness, "low").await;
    let mut filler: usize = 0;
    while harness
        .store()
        .set(&format!("filler:{filler}"), &filler)
        .await
        .is_ok()
    {
        filler += 1;
    }

    set(&harness, "high").await;
    assert!(
        guild_edits(&harness).is_empty(),
        "invites are not paused without a record that they were"
    );
    assert!(view(&harness).await.contains("Incomplete"));

    for n in 0..filler {
        harness
            .store()
            .delete(&format!("filler:{n}"))
            .await
            .unwrap();
    }
    harness.advance(std::time::Duration::from_secs(60));
    harness.run_due_jobs().await;
    assert_eq!(
        guild_edits(&harness),
        vec![Some(true)],
        "the resume job pauses the invites once the record can be written"
    );
    assert!(!view(&harness).await.contains("Incomplete"));
}

#[tokio::test]
async fn a_quarantined_member_stays_recorded_when_its_marker_role_cannot_be_given() {
    const MARKER: u64 = 78;
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": {
            "role": ROLE.to_string(),
            "marker_role": MARKER.to_string()
        } } }
    }));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(ROLE, "Quarantine")]);
    harness
        .api()
        .fail_role(Id::new(MARKER), DiscordError::Forbidden("no".into()));
    set(&harness, "high").await;

    assert!(
        harness.emit(joining(MEMBER)).await.is_err(),
        "the failed marker role is reported"
    );
    assert!(role_changes(&harness).contains(&(true, MEMBER, ROLE)));

    harness
        .invoke(sub("antiraid release").option("user", CommandOptionValue::User(Id::new(MEMBER))))
        .await
        .unwrap();
    assert!(
        role_changes(&harness).contains(&(false, MEMBER, ROLE)),
        "the quarantine role is taken off although the marker failed"
    );
}

#[tokio::test]
async fn a_join_of_a_member_who_is_still_recorded_gives_the_role_again() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": { "role": ROLE.to_string() } } }
    }));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(ROLE, "Quarantine")]);
    set(&harness, "high").await;
    join(&harness, MEMBER).await;
    // The record is still there, as if the leave had not forgotten the member.
    join(&harness, MEMBER).await;
    let given = role_changes(&harness)
        .iter()
        .filter(|change| **change == (true, MEMBER, ROLE))
        .count();
    assert_eq!(given, 2, "the role is given at each join");
}

#[tokio::test]
async fn the_quarantine_refuses_a_join_past_the_capacity_of_its_chunks() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": { "role": ROLE.to_string() } } }
    }));
    harness
        .api()
        .set_roles(Id::new(GUILD), vec![role(ROLE, "Quarantine")]);
    set(&harness, "high").await;
    // 17 chunks of 294 members is the whole capacity: no chunk has room left.
    for index in 0..17_u64 {
        let members: Vec<Value> = (0..294_u64)
            .map(|n| json!({ "user": 1_000_000 + index * 1_000 + n, "role": ROLE, "marker": 0 }))
            .collect();
        harness
            .store()
            .set(&format!("antiraid:quarantined:{index}"), &members)
            .await
            .unwrap();
    }

    assert!(
        harness.emit(joining(MEMBER)).await.is_err(),
        "a join past the capacity is refused"
    );
    assert!(!role_changes(&harness).contains(&(true, MEMBER, ROLE)));
}

#[tokio::test]
async fn an_expiry_after_the_section_is_removed_still_reverts_the_stored_changes() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "slowmode": { "seconds": 30 } } }
    }));
    harness
        .invoke(set_invocation("high").option("duration", CommandOptionValue::String("1m".into())))
        .await
        .unwrap();
    harness.set_config(json!({ "rules": {} }));

    harness.advance(std::time::Duration::from_secs(120));
    let results = harness.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok));
    assert!(
        channel_edits(&harness).contains(&(CHANNEL, Some(3))),
        "the timed slowmode goes back to the stored original: {:?}",
        channel_edits(&harness)
    );
    assert_eq!(
        texts_of_channel(&harness, LOG, "not configured any more"),
        1,
        "the log channel is told"
    );
    let level: Value = harness
        .store()
        .get("antiraid:level")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(level["name"], "calm");
}

#[tokio::test]
async fn a_level_that_quarantines_sets_up_its_role_when_it_is_set() {
    let harness = raid(json!({
        "levels": LEVELS,
        "effects": { "high": { "quarantine": {} } }
    }));
    set(&harness, "high").await;
    assert!(
        harness
            .api()
            .actions()
            .iter()
            .any(|action| matches!(action, Action::CreateRole { .. })),
        "the role is created with the level, not when the first member joins"
    );
    assert!(view(&harness).await.contains("still being set up"));

    for _ in 0..3 {
        harness.run_due_jobs().await;
    }
    assert!(view(&harness).await.contains("is ready"));
}
