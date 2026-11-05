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

#[tokio::test]
async fn welcome_truncates_expanded_content_at_the_character_limit() {
    let harness = harness();
    harness
        .api()
        .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
    harness
        .api()
        .set_guild_name(GuildId::new(1), &"あ".repeat(20));
    harness.set_config(serde_json::json!({
        "channel": "42",
        "message": format!("{}{{user}}{{server}}", "あ".repeat(1_979))
    }));

    harness
        .emit(join(123_456_789_012_345_678, false))
        .await
        .unwrap();

    let content = harness
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::SendMessage { content, .. } => content,
            _ => None,
        })
        .unwrap();
    assert_eq!(content.chars().count(), 2_000);
    assert!(content.ends_with('>'));
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
async fn welcome_delete_only_retries_transient_failures() {
    use closure_core::error::DiscordError;

    for (error, retry) in [
        (DiscordError::Transient("temporary".into()), true),
        (DiscordError::RateLimited { retry_after: None }, true),
        (DiscordError::NotFound, false),
        (DiscordError::Forbidden("denied".into()), false),
    ] {
        let harness = harness();
        harness
            .api()
            .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
        harness.set_config(serde_json::json!({
            "channel": "42",
            "message": "hello",
            "delete_after": "5s"
        }));
        harness.emit(join(7, false)).await.unwrap();
        harness.api().fail_with(error);
        harness.advance(Duration::from_secs(5));

        let results = harness.run_due_jobs().await;
        assert_eq!(results.iter().all(Result::is_ok), !retry);
    }
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

#[tokio::test]
async fn welcome_can_be_a_message_with_an_embed() {
    let harness = harness();
    harness.set_config(serde_json::json!({
        "channel": "42",
        "message": {
            "content": "Hi {user}",
            "embeds": [{"title": "Welcome to {server}", "color": "#5865f2", "footer": {"text": "{username}"}}]
        }
    }));
    harness
        .api()
        .add_channel(ChannelId::new(42), Some(GuildId::new(1)));
    harness.api().set_guild_name(GuildId::new(1), "Example");

    harness.emit(join(7, false)).await.unwrap();

    assert!(harness.actions().iter().any(|action| matches!(
        action,
        Action::SendMessage { content: Some(content), embeds, mentions: MentionPolicy::Users(users), .. }
            if content == "Hi <@7>"
                && users == &vec![UserId::new(7)]
                && embeds.len() == 1
                && embeds[0].title.as_deref() == Some("Welcome to Example")
                && embeds[0].color == Some(0x58_65f2)
                && embeds[0].footer.as_ref().is_some_and(|f| f.text == "user-7")
    )));
}

#[test]
fn a_message_with_an_unknown_variable_or_nothing_in_it_is_refused() {
    use crate::{Message, read_message};
    use closure_core::message::MessageSpec;
    let read = |value: serde_json::Value| {
        read_message(&Message::Rich(
            serde_json::from_value::<MessageSpec>(value).unwrap(),
        ))
    };
    assert!(read(serde_json::json!({"embeds": [{"title": "{user}"}]})).is_ok());
    assert!(read(serde_json::json!({"embeds": [{"title": "{nobody}"}]})).is_err());
    assert!(read(serde_json::json!({})).is_err());
}
