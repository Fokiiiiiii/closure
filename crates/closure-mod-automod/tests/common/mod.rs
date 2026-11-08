//! Helpers shared by the automod tests: the module on the SDK's pretend Discord, rules written as
//! JSON, and what the pretend Discord recorded.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a broken setup should panic at once"
)]
#![allow(
    dead_code,
    reason = "each test file uses its own part of these helpers"
)]
#![allow(
    clippy::needless_pass_by_value,
    reason = "helpers take the JSON they embed by value, which keeps the rules readable at the call site"
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use closure_core::action::Action;
use closure_mod_automod::builder;
use closure_sdk::testing::{Harness, Message, Reply};
use closure_sdk::{AttachmentInfo, Case, Event, MessageInfo, SavedMessage, UserInfo};
use serde_json::{Value, json};
use twilight_model::channel::message::Embed;
use twilight_model::guild::Member;
use twilight_model::id::Id;
use twilight_model::util::Timestamp;

pub const GUILD: u64 = 1;
pub const CHANNEL: u64 = 5;
pub const OTHER_CHANNEL: u64 = 6;
pub const LOG: u64 = 50;
pub const NOTIFY: u64 = 60;
pub const MODERATOR_ROLE: u64 = 10;
pub const ROLE: u64 = 77;
pub const MODERATOR: u64 = 100;
pub const MEMBER: u64 = 7;
pub const OTHER_MEMBER: u64 = 8;
pub const BOT: u64 = 9_999;

pub fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR_ROLE), closure_sdk::Level::MODERATOR);
    });
    for channel in [CHANNEL, OTHER_CHANNEL, NOTIFY] {
        harness
            .api()
            .add_channel(Id::new(channel), Some(Id::new(GUILD)));
    }
    harness.api().set_roles(
        Id::new(GUILD),
        vec![plain_role(ROLE), plain_role(OTHER_ROLE)],
    );
    harness
}

pub const OTHER_ROLE: u64 = 78;

pub fn plain_role(id: u64) -> twilight_model::guild::Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": format!("role-{id}"), "color": 0, "hoist": false,
        "managed": false, "mentionable": false, "permissions": "0", "position": 1, "flags": 0,
        "icon": null, "unicode_emoji": null, "tags": null,
        "colors": { "primary_color": 0, "secondary_color": null, "tertiary_color": null }
    }))
    .unwrap()
}

pub fn configured(config: Value) -> Harness {
    let harness = harness();
    harness.set_log_channel(LOG);
    harness.set_config(config);
    harness
}

pub fn with_rules(rules: Value) -> Harness {
    configured(json!({ "rules": rules }))
}

pub fn rules(list: &[(&str, Value)]) -> Value {
    Value::Object(
        list.iter()
            .map(|(id, rule)| ((*id).to_owned(), rule.clone()))
            .collect(),
    )
}

pub fn rule(triggers: Value, actions: Value) -> Value {
    json!({ "triggers": triggers, "actions": actions })
}

pub fn delete() -> Value {
    json!([{ "do": "delete" }])
}

pub fn deletes_when(trigger: Value) -> Harness {
    with_rules(rules(&[("r", rule(json!([trigger]), delete()))]))
}

pub fn scoped(mut rule: Value, scope: Value) -> Value {
    rule["scope"] = scope;
    rule
}

pub async fn say(harness: &Harness, author: u64, content: &str) {
    say_in(harness, author, CHANNEL, content).await;
}

pub async fn say_in(harness: &Harness, author: u64, channel: u64, content: &str) {
    harness
        .message(Message::new(author, channel).content(content))
        .await
        .unwrap();
}

pub fn custom(author: u64, channel: u64, content: &str) -> MessageInfo {
    static NEXT: AtomicU64 = AtomicU64::new(50_000);
    MessageInfo {
        removed_attachments: Vec::new(),
        previous_content: None,
        id: Id::new(NEXT.fetch_add(1, Ordering::Relaxed)),
        channel_id: Id::new(channel),
        guild_id: Id::new(GUILD),
        author: UserInfo {
            id: Id::new(author),
            name: "tester".into(),
            bot: false,
        },
        author_roles: Vec::new(),
        content: content.to_owned(),
        mentioned_users: Vec::new(),
        mentioned_roles: Vec::new(),
        mentions_everyone: false,
        attachments: Vec::new(),
        embed_count: 0,
        embeds_text: String::new(),
        sticker_count: 0,
        reply_to: None,
        from_webhook: false,
        created_at: 0,
        edited_at: None,
    }
}

pub fn file(name: &str, content_type: Option<&str>) -> AttachmentInfo {
    AttachmentInfo {
        id: 1,
        filename: name.into(),
        size: 10,
        content_type: content_type.map(str::to_owned),
        url: String::new(),
    }
}

pub async fn post(harness: &Harness, message: MessageInfo) {
    harness.emit(Event::MessageCreated(message)).await.unwrap();
}

pub async fn trips_with(trigger: Value, info: MessageInfo) -> bool {
    let harness = deletes_when(trigger);
    post(&harness, info).await;
    deletions(&harness) > 0
}

pub async fn trips(trigger: Value, content: &str) -> bool {
    trips_with(trigger, custom(MEMBER, CHANNEL, content)).await
}

pub fn deleted_ids(harness: &Harness) -> Vec<u64> {
    harness
        .api()
        .actions()
        .iter()
        .flat_map(|action| match action {
            Action::DeleteMessage { message_id, .. } => vec![message_id.get()],
            Action::BulkDeleteMessages { message_ids, .. } => {
                message_ids.iter().map(|id| id.get()).collect()
            }
            _ => Vec::new(),
        })
        .collect()
}

pub fn deletions(harness: &Harness) -> usize {
    deleted_ids(harness).len()
}

pub fn count_actions(harness: &Harness, wanted: impl Fn(&Action) -> bool) -> usize {
    harness.api().actions().iter().filter(|a| wanted(a)).count()
}

pub fn sent_texts(harness: &Harness) -> Vec<(u64, String)> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                channel_id,
                content,
                embeds,
                ..
            } => content
                .or_else(|| embeds.into_iter().find_map(|embed| embed.description))
                .map(|text| (channel_id.get(), text)),
            _ => None,
        })
        .collect()
}

pub async fn cases_of(harness: &Harness, target: u64) -> Vec<Case> {
    harness
        .cases()
        .for_target(Id::new(target), None, 25)
        .await
        .unwrap()
}

pub fn embed_text(embed: &Embed) -> String {
    let mut text = String::new();
    if let Some(title) = &embed.title {
        text.push_str(title);
    }
    if let Some(description) = &embed.description {
        text.push('\n');
        text.push_str(description);
    }
    for field in &embed.fields {
        text.push('\n');
        text.push_str(&field.name);
        text.push(' ');
        text.push_str(&field.value);
    }
    text
}

pub fn description(replies: &[Reply]) -> String {
    replies
        .iter()
        .flat_map(|reply| reply.embeds.iter())
        .filter_map(|embed| embed.description.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn seed(harness: &Harness, id: u64, author: u64, channel: u64, seconds_ago: i64) {
    let created_at = unix_now() - seconds_ago;
    harness
        .seed_message(SavedMessage {
            id: Id::new(id),
            channel_id: Id::new(channel),
            guild_id: Id::new(GUILD),
            author: Id::new(author),
            author_bot: false,
            content: "earlier".into(),
            attachments: Vec::new(),
            embed_count: 0,
            sticker_count: 0,
            mentioned_users: Vec::new(),
            mentioned_roles: Vec::new(),
            mentions_everyone: false,
            reply_to: None,
            created_at,
            edited_at: None,
            deleted_at: None,
        })
        .await
        .unwrap();
}

pub fn unix_now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

pub fn member(id: u64, roles: &[u64], bot: bool, joined: Option<&str>) -> Member {
    let mut value = json!({
        "deaf": false,
        "mute": false,
        "flags": 0,
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "user": {
            "id": id.to_string(),
            "username": "member",
            "discriminator": "0",
            "avatar": null,
            "bot": bot,
        },
    });
    if let Some(joined) = joined {
        value["joined_at"] = json!(joined);
    }
    serde_json::from_value(value).unwrap()
}

pub fn days_ago(days: i64) -> String {
    let secs = unix_now() - days * 86_400;
    Timestamp::from_secs(secs).unwrap().iso_8601().to_string()
}

pub fn declare(harness: &Harness, id: u64, roles: &[u64]) {
    harness
        .api()
        .set_member(Id::new(GUILD), member(id, roles, false, None));
}

pub async fn join_with(harness: &Harness, user: u64, roles: &[u64], joined_at: i64) {
    harness
        .emit(Event::MemberJoined(closure_core::event::MemberInfo {
            guild_id: Id::new(GUILD),
            user: UserInfo {
                id: Id::new(user),
                name: "member".into(),
                bot: false,
            },
            roles: roles.iter().map(|id| Id::new(*id)).collect(),
            nick: None,
            joined_at: Some(joined_at),
            timed_out_until: None,
        }))
        .await
        .unwrap();
}

pub async fn join(harness: &Harness, user: u64) {
    join_with(harness, user, &[], unix_now()).await;
}

pub async fn leave(harness: &Harness, user: u64) {
    harness
        .emit(Event::MemberLeft {
            guild_id: Id::new(GUILD),
            user: UserInfo {
                id: Id::new(user),
                name: "member".into(),
                bot: false,
            },
        })
        .await
        .unwrap();
}

pub fn account_made(secs_ago: i64) -> u64 {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    let made_ms = u64::try_from((unix_now() - secs_ago) * 1000).unwrap();
    ((made_ms - DISCORD_EPOCH_MS) << 22) | 1
}

pub fn case(kind: &str, target: u64, moderator: u64) -> Case {
    Case {
        number: 1,
        guild_id: Id::new(GUILD),
        module: closure_core::module::ModuleId::new("moderation").unwrap(),
        kind: closure_sdk::CaseKind::new(kind).unwrap(),
        target: Id::new(target),
        moderator: Id::new(moderator),
        reason: None,
        created_at: unix_now(),
        expires_at: None,
        revoked: None,
        source: closure_sdk::CaseSource::Command,
        hidden: false,
    }
}

pub fn texts_in(harness: &Harness, channel: u64) -> Vec<String> {
    sent_texts(harness)
        .into_iter()
        .filter(|(sent_to, _)| *sent_to == channel)
        .map(|(_, text)| text)
        .collect()
}
