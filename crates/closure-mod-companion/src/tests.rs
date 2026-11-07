use closure_core::action::{Action, OverwriteTarget};
use closure_core::event::{ChannelId, Event, GuildId, UserId, UserInfo};
use closure_sdk::testing::Harness;

use crate::builder;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness
        .api()
        .add_channel(ChannelId::new(31), Some(GuildId::new(1)));
    harness.set_config(serde_json::json!({"pairs": [{"voice": "30", "text": "31"}]}));
    harness
}

fn voice(user: u64, before: Option<u64>, after: Option<u64>) -> Event {
    Event::VoiceStateChanged {
        guild_id: GuildId::new(1),
        user: UserInfo {
            id: UserId::new(user),
            name: "member".into(),
            bot: false,
        },
        before: before.map(ChannelId::new),
        after: after.map(ChannelId::new),
    }
}

fn shown(harness: &Harness) -> Vec<(u64, u64)> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::SetPermissionOverwrite {
                channel_id,
                overwrite,
                ..
            } => match overwrite.target {
                OverwriteTarget::Member(user) => Some((channel_id.get(), user.get())),
                OverwriteTarget::Role(_) => None,
            },
            _ => None,
        })
        .collect()
}

fn hidden(harness: &Harness) -> Vec<(u64, u64)> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::DeletePermissionOverwrite {
                channel_id,
                target: OverwriteTarget::Member(user),
                ..
            } => Some((channel_id.get(), user.get())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_text_channel_is_shown_to_whoever_is_in_the_voice_channel() {
    let harness = harness();
    harness.emit(voice(7, None, Some(30))).await.unwrap();
    assert_eq!(shown(&harness), [(31, 7)]);
    harness.emit(voice(7, Some(30), None)).await.unwrap();
    assert_eq!(hidden(&harness), [(31, 7)]);
}

#[tokio::test]
async fn other_voice_channels_and_staying_put_change_nothing() {
    let harness = harness();
    harness.emit(voice(7, None, Some(99))).await.unwrap();
    harness.emit(voice(7, Some(30), Some(30))).await.unwrap();
    assert!(harness.actions().is_empty());
}

#[tokio::test]
async fn what_the_member_had_of_their_own_is_given_back_and_not_deleted() {
    let harness = harness();
    // The member has an overwrite of their own on the text channel: it allows 64 (add reactions).
    let channel = serde_json::from_value(serde_json::json!({
        "id": "31", "guild_id": "1", "type": 0, "name": "voice-text",
        "permission_overwrites": [{ "id": "7", "type": 1, "allow": "64", "deny": "0" }]
    }))
    .unwrap();
    harness.api().set_channels(GuildId::new(1), vec![channel]);
    harness.emit(voice(7, None, Some(30))).await.unwrap();
    harness.emit(voice(7, Some(30), None)).await.unwrap();
    assert!(
        hidden(&harness).is_empty(),
        "their own overwrite is not deleted"
    );
    let set: Vec<u64> = harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::SetPermissionOverwrite { overwrite, .. } => Some(overwrite.allow.bits()),
            _ => None,
        })
        .collect();
    assert_eq!(set.len(), 2);
    assert_eq!(set[1], 64, "given back as it was");
    assert_ne!(
        set[0] & 64,
        0,
        "and kept while they are in the voice channel"
    );
}

fn set_allows(harness: &Harness) -> Vec<u64> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::SetPermissionOverwrite { overwrite, .. } => Some(overwrite.allow.bits()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn moving_between_voice_channels_of_one_text_channel_keeps_what_the_member_really_had() {
    let harness = harness();
    harness.set_config(serde_json::json!({"pairs": [
        {"voice": "30", "text": "31"}, {"voice": "32", "text": "31"}
    ]}));
    // The overwrite the module makes shows up in the channel as it is read the second time.
    let with_shown = serde_json::from_value(serde_json::json!({
        "id": "31", "guild_id": "1", "type": 0, "name": "voice-text",
        "permission_overwrites": [{ "id": "7", "type": 1, "allow": "68608", "deny": "0" }]
    }))
    .unwrap();
    harness.emit(voice(7, None, Some(30))).await.unwrap();
    harness
        .api()
        .set_channels(GuildId::new(1), vec![with_shown]);
    harness.emit(voice(7, Some(30), Some(32))).await.unwrap();
    harness.emit(voice(7, Some(32), None)).await.unwrap();
    assert_eq!(
        hidden(&harness),
        [(31, 7)],
        "they had nothing of their own: the overwrite goes, it is not given back"
    );
    assert_eq!(
        set_allows(&harness).len(),
        2,
        "shown twice, never given back"
    );
}

#[tokio::test]
async fn a_text_channel_that_cannot_be_read_is_left_alone() {
    let harness = harness();
    harness
        .api()
        .fail_nth(1, closure_core::error::DiscordError::Forbidden("no".into()));
    assert!(harness.emit(voice(7, None, Some(30))).await.is_err());
    assert!(
        set_allows(&harness).is_empty(),
        "an overwrite that could not be read is not written over"
    );
}

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(
        closure_i18n::missing_keys(crate::EN, crate::JA).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        closure_i18n::missing_keys(crate::JA, crate::EN).unwrap(),
        Vec::<String>::new()
    );
}
