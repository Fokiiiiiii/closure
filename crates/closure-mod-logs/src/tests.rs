use closure_core::action::Action;
use closure_core::capability::Capability;
use closure_core::error::DiscordError;
use closure_core::event::{AttachmentInfo, Event, MemberInfo, MessageInfo, UserInfo};
use closure_core::history::SavedMessage;
use closure_core::module::Module;
use closure_sdk::Locale;
use closure_sdk::testing::Harness;
use serde_json::json;
use twilight_model::id::Id;

use super::*;

const GUILD: u64 = 1;
const CHANNEL: u64 = 3;
const LOG: u64 = 77;
const OTHER_LOG: u64 = 88;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.set_log_channel(LOG);
    harness
}

fn config(channel: Option<&str>, deletes: bool) -> serde_json::Value {
    json!({
        "channel": channel,
        "message_edits": true,
        "message_deletes": deletes,
        "member_joins": true,
        "member_leaves": true,
        "bans": true,
        "keep_attachments": false
    })
}

/// A 1 by 1 PNG, the picture that the showcase shows inside its embed.
const PICTURE_BYTES: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x64, 0x60, 0xF8, 0x5F,
    0x0F, 0x00, 0x02, 0x87, 0x01, 0x80, 0xEB, 0x47, 0xBA, 0x92, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

fn picture(id: u64) -> AttachmentInfo {
    AttachmentInfo {
        id,
        filename: "photo.png".into(),
        size: 2048,
        content_type: Some("image/png".into()),
        url: format!("https://cdn.discordapp.com/attachments/{CHANNEL}/{id}/photo.png"),
    }
}

fn saved(id: u64, content: &str, attachments: Vec<AttachmentInfo>) -> SavedMessage {
    SavedMessage {
        id: Id::new(id),
        channel_id: Id::new(CHANNEL),
        guild_id: Id::new(GUILD),
        author: Id::new(5),
        author_bot: false,
        content: content.into(),
        attachments,
        embed_count: 0,
        sticker_count: 0,
        mentioned_users: Vec::new(),
        mentioned_roles: Vec::new(),
        mentions_everyone: false,
        reply_to: None,
        created_at: 1_700_000_000,
        edited_at: None,
        deleted_at: None,
    }
}

fn deleted(id: u64) -> Event {
    Event::MessageDeleted {
        guild_id: Id::new(GUILD),
        channel_id: Id::new(CHANNEL),
        message_id: Id::new(id),
    }
}

fn edited(
    id: u64,
    content: &str,
    attachments: Vec<AttachmentInfo>,
    removed: Vec<AttachmentInfo>,
) -> Event {
    Event::MessageUpdated(MessageInfo {
        id: Id::new(id),
        channel_id: Id::new(CHANNEL),
        guild_id: Id::new(GUILD),
        author: UserInfo {
            id: Id::new(5),
            name: "tester".into(),
            bot: false,
        },
        author_roles: Vec::new(),
        content: content.into(),
        mentioned_users: Vec::new(),
        mentioned_roles: Vec::new(),
        mentions_everyone: false,
        attachments,
        removed_attachments: removed,
        previous_content: None,
        embed_count: 0,
        sticker_count: 0,
        reply_to: None,
        from_webhook: false,
        created_at: 1_700_000_000,
        edited_at: Some(1_700_000_100),
    })
}

/// The edit as the recorder hands it on: the text it replaced is `previous`.
fn replacing(mut event: Event, previous: &str) -> Event {
    if let Event::MessageUpdated(info) = &mut event {
        info.previous_content = Some(previous.into());
    }
    event
}

/// What the module posted: the channel, the text, the JSON of the embeds, and each file's name and bytes.
struct Posted {
    channel: u64,
    text: Option<String>,
    embeds: String,
    files: Vec<(String, Vec<u8>)>,
}

fn sent(harness: &Harness) -> Vec<Posted> {
    harness
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                channel_id,
                content,
                embeds,
                ..
            } => Some(Posted {
                channel: channel_id.get(),
                text: content.clone(),
                embeds: serde_json::to_string(embeds).unwrap(),
                files: Vec::new(),
            }),
            Action::SendFile {
                channel_id,
                content,
                embeds,
                file,
            } => Some(Posted {
                channel: channel_id.get(),
                text: content.clone(),
                embeds: serde_json::to_string(embeds).unwrap(),
                files: vec![(file.filename.clone(), file.data.clone())],
            }),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_deleted_picture_is_posted_as_the_file_the_host_kept() {
    let harness = harness();
    harness
        .seed_message(saved(10, "look at this", vec![picture(1)]))
        .await
        .unwrap();
    harness
        .seed_file(CHANNEL, 10, picture(1), Some(b"the picture".to_vec()))
        .await
        .unwrap();

    harness.emit(deleted(10)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert_eq!(posted[0].channel, LOG);
    assert!(
        posted[0].text.is_none(),
        "the file is posted as a file, not as a link"
    );
    assert!(
        posted[0].embeds.contains("look at this"),
        "{}",
        posted[0].embeds
    );
    assert_eq!(
        posted[0].files,
        [("photo.png".to_owned(), b"the picture".to_vec())]
    );
    assert!(
        posted[0].embeds.contains("attachment://photo.png"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn a_deleted_picture_the_host_did_not_keep_is_named_as_not_kept() {
    let harness = harness();
    harness
        .seed_message(saved(11, "gone", vec![picture(2)]))
        .await
        .unwrap();
    harness
        .seed_file(CHANNEL, 11, picture(2), None)
        .await
        .unwrap();

    harness.emit(deleted(11)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(posted[0].files.is_empty());
    assert!(
        posted[0].embeds.contains("photo.png"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("not kept"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn a_deleted_message_the_history_did_not_keep_is_logged_as_not_kept() {
    let harness = harness();

    harness.emit(deleted(12)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("The content was not kept"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn an_edit_that_removes_a_picture_posts_the_picture() {
    let harness = harness();
    harness
        .seed_file(CHANNEL, 31, picture(4), Some(b"removed bytes".to_vec()))
        .await
        .unwrap();

    harness
        .emit(edited(31, "the new text", Vec::new(), vec![picture(4)]))
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("Removed by an edit"),
        "{}",
        posted[0].embeds
    );
    assert_eq!(
        posted[0].files,
        [("photo.png".to_owned(), b"removed bytes".to_vec())]
    );
}

#[tokio::test]
async fn an_edit_shows_the_new_text_and_its_attachments() {
    let harness = harness();

    harness
        .emit(edited(
            30,
            "now with a picture",
            vec![picture(3)],
            Vec::new(),
        ))
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("Message edited"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("now with a picture"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("photo.png"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn an_edit_that_only_changes_the_text_does_not_post_a_picture_an_earlier_edit_removed() {
    let harness = harness();
    harness
        .seed_file(CHANNEL, 32, picture(5), Some(b"removed bytes".to_vec()))
        .await
        .unwrap();

    harness
        .emit(edited(
            32,
            "the picture is gone",
            Vec::new(),
            vec![picture(5)],
        ))
        .await
        .unwrap();
    harness
        .emit(edited(
            32,
            "and the text changed again",
            Vec::new(),
            Vec::new(),
        ))
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 2, "{:?}", posted.len());
    assert_eq!(
        posted[0].files,
        [("photo.png".to_owned(), b"removed bytes".to_vec())]
    );
    assert!(
        posted[1].files.is_empty(),
        "the first edit posted the picture already: {:?}",
        posted[1].files
    );
}

#[tokio::test]
async fn deletions_are_not_logged_when_their_setting_is_off() {
    let harness = harness();
    harness.set_config(config(None, false));

    harness.emit(deleted(13)).await.unwrap();

    assert!(sent(&harness).is_empty());
}

#[tokio::test]
async fn the_entries_go_to_the_configured_channel_and_not_the_log_channel() {
    let harness = harness();
    // The harness knows only the channels it was given: register the other one, then put the
    // log channel back, so that the configured channel is the one that must win.
    harness.set_log_channel(OTHER_LOG);
    harness.set_log_channel(LOG);
    harness.set_config(config(Some(&OTHER_LOG.to_string()), true));

    harness.emit(deleted(14)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert_eq!(posted[0].channel, OTHER_LOG);
}

#[tokio::test]
async fn a_configured_channel_that_is_not_a_channel_is_an_error_and_nothing_is_posted() {
    for channel in ["0", "abc", ""] {
        let harness = harness();
        harness.set_config(config(Some(channel), true));

        let result = harness.emit(deleted(15)).await;

        assert!(
            matches!(result, Err(ModuleError::Config(_))),
            "{channel:?}: {result:?}"
        );
        assert!(sent(&harness).is_empty(), "{channel:?}");
    }
}

#[tokio::test]
async fn a_refused_post_is_a_discord_error_and_not_a_module_fault() {
    let harness = harness();
    harness
        .api()
        .fail_with(DiscordError::Forbidden("missing access".into()));

    let result = harness.emit(deleted(16)).await;

    assert!(
        matches!(
            result,
            Err(ModuleError::Discord(DiscordError::Forbidden(_)))
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn joins_leaves_and_bans_are_logged() {
    let harness = harness();
    let user = UserInfo {
        id: Id::new(40),
        name: "newcomer".into(),
        bot: false,
    };
    let member = MemberInfo {
        guild_id: Id::new(GUILD),
        user: user.clone(),
        roles: Vec::new(),
        nick: None,
        joined_at: None,
        timed_out_until: None,
    };

    harness.emit(Event::MemberJoined(member)).await.unwrap();
    harness
        .emit(Event::MemberLeft {
            guild_id: Id::new(GUILD),
            user: user.clone(),
        })
        .await
        .unwrap();
    harness
        .emit(Event::BanAdded {
            guild_id: Id::new(GUILD),
            user: user.clone(),
        })
        .await
        .unwrap();
    harness
        .emit(Event::BanRemoved {
            guild_id: Id::new(GUILD),
            user,
        })
        .await
        .unwrap();

    let texts: Vec<String> = sent(&harness)
        .into_iter()
        .map(|posted| posted.embeds)
        .collect();
    assert_eq!(texts.len(), 4, "{texts:?}");
    assert!(texts[0].contains("Member joined") && texts[0].contains("newcomer"));
    assert!(texts[1].contains("Member left"));
    assert!(texts[2].contains("Member banned"));
    assert!(texts[3].contains("Member unbanned"));
}

#[tokio::test]
async fn showcase_members_who_join_leave_and_are_banned_are_logged() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    let user = UserInfo {
        id: Id::new(40),
        name: "newcomer".into(),
        bot: false,
    };
    let member = MemberInfo {
        guild_id: Id::new(GUILD),
        user: user.clone(),
        roles: Vec::new(),
        nick: None,
        joined_at: None,
        timed_out_until: None,
    };

    harness.emit(Event::MemberJoined(member)).await.unwrap();
    harness
        .emit(Event::MemberLeft {
            guild_id: Id::new(GUILD),
            user: user.clone(),
        })
        .await
        .unwrap();
    harness
        .emit(Event::BanAdded {
            guild_id: Id::new(GUILD),
            user: user.clone(),
        })
        .await
        .unwrap();
    harness
        .emit(Event::BanRemoved {
            guild_id: Id::new(GUILD),
            user,
        })
        .await
        .unwrap();

    assert_eq!(sent(&harness).len(), 4);
}

#[tokio::test]
async fn showcase_an_edit_that_removes_a_picture_shows_the_picture() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness
        .seed_file(CHANNEL, 31, picture(4), Some(PICTURE_BYTES.to_vec()))
        .await
        .unwrap();

    harness
        .emit(replacing(
            edited(
                31,
                "写真を消して、文章だけ直しました。",
                Vec::new(),
                vec![picture(4)],
            ),
            "写真付きで投稿しました。",
        ))
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("attachment://photo.png"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn showcase_a_deleted_picture_is_shown_inside_its_embed() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness
        .seed_message(saved(
            30,
            "写真付きのメッセージを消しました。",
            vec![picture(7)],
        ))
        .await
        .unwrap();
    harness
        .seed_file(CHANNEL, 30, picture(7), Some(PICTURE_BYTES.to_vec()))
        .await
        .unwrap();

    harness.emit(deleted(30)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("attachment://photo.png"),
        "{}",
        posted[0].embeds
    );
    assert_eq!(
        posted[0].files,
        [("photo.png".to_owned(), PICTURE_BYTES.to_vec())]
    );
}

#[tokio::test]
async fn showcase_a_deleted_message_shows_its_text_and_when_it_was_posted() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness
        .seed_message(saved(
            20,
            "明日の会議の資料を共有します。確認をお願いします。",
            Vec::new(),
        ))
        .await
        .unwrap();

    harness.emit(deleted(20)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("明日の会議の資料"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn showcase_an_edit_shows_the_text_before_and_after_it() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);

    harness
        .emit(replacing(
            edited(
                21,
                "明日の会議は十時からに変更します。",
                Vec::new(),
                Vec::new(),
            ),
            "明日の会議は九時からです。",
        ))
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("九時からです"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("十時からに変更"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn showcase_a_bulk_deletion_shows_a_summary_and_each_message_it_kept() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness
        .seed_message(saved(
            22,
            "来週の発表資料に差し替えをお願いします。",
            Vec::new(),
        ))
        .await
        .unwrap();
    harness
        .seed_message(saved(23, "議事録のURLは後で共有します。", Vec::new()))
        .await
        .unwrap();

    harness
        .emit(Event::MessagesBulkDeleted {
            guild_id: Id::new(GUILD),
            channel_id: Id::new(CHANNEL),
            message_ids: vec![Id::new(22), Id::new(23)],
        })
        .await
        .unwrap();

    // The entries without files share one message: the summary, then each message kept.
    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("一括削除"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("差し替え"),
        "{}",
        posted[0].embeds
    );
    assert!(posted[0].embeds.contains("議事録"), "{}", posted[0].embeds);
}

/// A user id whose snowflake says the account was made at `unix_seconds`.
fn user_made_at(unix_seconds: u64) -> u64 {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    (((unix_seconds * 1_000) - DISCORD_EPOCH_MS) << 22) | 1
}

#[tokio::test]
async fn a_join_shows_when_the_account_was_made_and_when_the_member_joined() {
    let harness = harness();
    let member = MemberInfo {
        guild_id: Id::new(GUILD),
        user: UserInfo {
            id: Id::new(user_made_at(1_600_000_000)),
            name: "newcomer".into(),
            bot: false,
        },
        roles: Vec::new(),
        nick: None,
        joined_at: Some(1_650_000_000),
        timed_out_until: None,
    };

    harness.emit(Event::MemberJoined(member)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("<t:1600000000:f>"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("<t:1650000000:f>"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn a_deleted_message_shows_its_id_and_when_it_was_posted_and_is_stamped_with_the_deletion() {
    let harness = harness();
    harness
        .seed_message(saved(10, "look at this", Vec::new()))
        .await
        .unwrap();

    harness.emit(deleted(10)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains(r#""value":"10""#),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("<t:1700000000:f>"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("\"timestamp\""),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn an_edit_shows_the_text_before_and_after_it() {
    let harness = harness();
    harness
        .emit(replacing(
            edited(33, "the new text", Vec::new(), Vec::new()),
            "the old text",
        ))
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("the old text"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("the new text"),
        "{}",
        posted[0].embeds
    );
    assert!(posted[0].embeds.contains("Before"), "{}", posted[0].embeds);
    assert!(posted[0].embeds.contains("After"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn an_edit_whose_text_before_it_the_history_did_not_keep_says_so() {
    let harness = harness();

    harness
        .emit(edited(34, "the new text", Vec::new(), Vec::new()))
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("The content was not kept"),
        "{}",
        posted[0].embeds
    );
}

#[test]
fn the_module_reads_the_history_and_keeps_files_it_asks_for() {
    let module = module().unwrap();

    let capabilities = &module.manifest().capabilities;
    assert!(capabilities.contains(Capability::ReadMessageHistory));
    assert!(capabilities.contains(Capability::KeepAttachments));
}

#[test]
fn the_module_asks_for_message_content_so_that_it_can_post_the_text() {
    let module = module().unwrap();

    assert!(module.manifest().intents.contains(Intents::MESSAGE_CONTENT));
}

#[test]
fn both_languages_define_the_same_messages() {
    fn keys(text: &str) -> Vec<&str> {
        let mut keys: Vec<&str> = text
            .lines()
            .filter_map(|line| line.split_once(" =").map(|(key, _)| key))
            .collect();
        keys.sort_unstable();
        keys
    }

    assert!(!keys(EN).is_empty());
    assert_eq!(keys(EN), keys(JA));
}
