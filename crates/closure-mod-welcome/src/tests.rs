use std::time::Duration;

use closure_core::action::{Action, MentionPolicy};
use closure_core::event::{ChannelId, Event, GuildId, MemberInfo, UserId, UserInfo};
use closure_sdk::Locale;
use closure_sdk::testing::Harness;

use crate::{builder, validate_delete_after, validate_template};

fn join(user: u64, bot: bool) -> Event {
    Event::MemberJoined(MemberInfo {
        guild_id: GuildId::new(1),
        user: UserInfo {
            id: UserId::new(user),
            name: format!("user-{user}"),
            bot,
        },
        roles: Vec::new(),
        nick: None,
        joined_at: None,
        timed_out_until: None,
    })
}

fn harness() -> Harness {
    Harness::new(builder().unwrap()).unwrap()
}

#[tokio::test]
async fn welcome_uses_the_localized_catalog_default() {
    let harness = harness();
    harness
        .api()
        .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
    harness.api().set_guild_name(GuildId::new(1), "Example");
    harness.set_guild_locale(Locale::Ja);
    harness.set_config(serde_json::json!({ "channel": "42" }));

    harness.emit(join(7, false)).await.unwrap();

    assert!(harness.actions().iter().any(|action| matches!(
        action,
        Action::SendMessage { content: Some(content), .. }
            if content == "Exampleへようこそ、<@7>さん！"
    )));
}

#[tokio::test]
async fn welcome_sends_a_localized_template_and_only_allows_the_member_mention() {
    let harness = harness();
    harness.set_config(serde_json::json!({
        "channel": "42",
        "message": "Hi {user} ({username}) in {server}, count={count}",
        "include_bots": false
    }));
    harness
        .api()
        .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
    harness.api().set_guild_name(GuildId::new(1), "Example");

    harness.emit(join(7, false)).await.unwrap();

    assert!(harness.actions().iter().any(|action| matches!(
        action,
        Action::SendMessage {
            channel_id,
            content: Some(content),
            mentions: MentionPolicy::Users(users),
            ..
        } if channel_id.get() == 42
            && content == "Hi <@7> (user-7) in Example, count="
            && users == &vec![UserId::new(7)]
    )));
}

#[test]
fn welcome_templates_and_delete_durations_are_validated() {
    assert!(validate_template("Welcome {username}").is_ok());
    assert!(validate_template("{unknown}").is_err());
    assert!(validate_template(&"x".repeat(2_001)).is_err());
    assert!(validate_template("{unterminated").is_err());
    assert_eq!(validate_delete_after("5s").unwrap(), Duration::from_secs(5));
    assert_eq!(
        validate_delete_after("7d").unwrap(),
        Duration::from_hours(168)
    );
    assert!(validate_delete_after("4s").is_err());
    assert!(validate_delete_after("8d").is_err());
    assert!(validate_delete_after("604800s").is_ok());
    assert!(validate_delete_after("604801s").is_err());
}

#[tokio::test]
async fn welcome_ignores_bots_unless_enabled() {
    let harness = harness();
    harness
        .api()
        .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
    harness.set_config(serde_json::json!({ "channel": "42" }));
    harness.emit(join(7, true)).await.unwrap();
    assert!(
        !harness
            .actions()
            .iter()
            .any(|action| matches!(action, Action::SendMessage { .. }))
    );

    harness.set_config(serde_json::json!({ "channel": "42", "include_bots": true }));
    harness.emit(join(8, true)).await.unwrap();
    assert!(harness.actions().iter().any(|action| matches!(
        action,
        Action::SendMessage { content: Some(content), .. } if content.contains("<@8>")
    )));
}

#[tokio::test]
async fn welcome_delete_after_schedules_and_runs_a_delete_job() {
    let harness = harness();
    harness
        .api()
        .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
    harness.set_config(serde_json::json!({
        "channel": "42",
        "message": "hello {username}",
        "delete_after": "5s"
    }));

    harness.emit(join(7, false)).await.unwrap();
    assert_eq!(harness.jobs(), 1);
    harness.advance(Duration::from_secs(5));
    assert!(harness.run_due_jobs().await.iter().all(Result::is_ok));
    assert!(harness.actions().iter().any(|action| matches!(
        action,
        Action::DeleteMessage { channel_id, message_id, .. }
            if channel_id.get() == 42 && message_id.get() > 0
    )));
}

#[tokio::test]
async fn welcome_sends_at_most_five_greetings_in_one_minute() {
    let harness = harness();
    harness
        .api()
        .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
    harness.set_config(serde_json::json!({ "channel": "42", "message": "hello" }));

    for user in 1..=6 {
        harness.emit(join(user, false)).await.unwrap();
    }

    assert_eq!(
        harness
            .actions()
            .iter()
            .filter(|action| matches!(action, Action::SendMessage { .. }))
            .count(),
        5
    );
}

#[tokio::test]
async fn welcome_without_channel_does_nothing_and_send_failure_does_not_fail_the_event() {
    let harness = harness();
    harness.emit(join(7, false)).await.unwrap();
    assert!(harness.actions().is_empty());

    harness.set_config(serde_json::json!({ "channel": "42", "message": "hello" }));
    harness
        .api()
        .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
    harness
        .api()
        .fail_with(closure_core::error::DiscordError::Forbidden(
            "denied".into(),
        ));
    assert!(harness.emit(join(8, false)).await.is_ok());
}
