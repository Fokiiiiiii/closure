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
