use std::time::{SystemTime, UNIX_EPOCH};

use closure_core::action::Action;
use closure_core::capability::Capability;
use closure_core::error::DiscordError;
use closure_core::event::{
    AttachmentInfo, ChannelInfo, EmojiInfo, Event, MemberInfo, MessageInfo, OverwriteInfo,
    OverwriteTarget, RoleInfo, UserInfo,
};
use closure_core::history::SavedMessage;
use closure_core::module::Module;
use closure_sdk::Locale;
use closure_sdk::testing::Harness;
use serde_json::json;
use twilight_model::guild::Permissions;
use twilight_model::guild::audit_log::{AuditLogEntry, AuditLogEventType};
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
        "channels": true,
        "threads": true,
        "roles": true,
        "member_roles": true,
        "member_nicknames": true,
        "member_timeouts": true,
        "voice": false,
        "emojis": true,
        "stickers": true,
        "stage_instances": true,
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

/// A channel as the gateway describes it, with the topic it has.
fn channel_info(name: &str, topic: Option<&str>) -> ChannelInfo {
    ChannelInfo {
        id: Id::new(CHANNEL),
        guild_id: Id::new(GUILD),
        name: Some(name.into()),
        parent_id: None,
        topic: topic.map(Into::into),
        nsfw: false,
        overwrites: Vec::new(),
    }
}

fn member(id: u64) -> UserInfo {
    UserInfo {
        id: Id::new(id),
        name: "member".into(),
        bot: false,
    }
}

fn overwrite_audit(kind: AuditLogEventType, channel: u64, target: u64, by: u64) -> AuditLogEntry {
    let mut entry = audit(kind, channel, by, None, 5);
    entry.options = Some(
        serde_json::from_value(
            json!({ "channel_id": channel.to_string(), "id": target.to_string() }),
        )
        .unwrap(),
    );
    entry
}

fn role_overwrite(role: u64, allow: Permissions, deny: Permissions) -> OverwriteInfo {
    OverwriteInfo {
        target: OverwriteTarget::Role(Id::new(role)),
        allow,
        deny,
    }
}

#[tokio::test]
async fn a_change_of_a_permission_overwrite_shows_what_the_role_allows_and_denies() {
    let harness = harness();
    let mut after = channel_info("general", None);
    after.overwrites = vec![role_overwrite(
        9,
        Permissions::VIEW_CHANNEL,
        Permissions::SEND_MESSAGES,
    )];

    harness
        .emit(Event::ChannelChanged {
            before: channel_info("general", None),
            after,
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(
        posted[0].embeds.contains("Permission overwrite"),
        "{}",
        posted[0].embeds
    );
    assert!(posted[0].embeds.contains("<@&9>"), "{}", posted[0].embeds);
    assert!(
        posted[0].embeds.contains("+VIEW_CHANNEL"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("+SEND_MESSAGES"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn a_role_whose_permissions_changed_shows_the_permissions_added_and_removed() {
    let harness = harness();
    let role = |permissions: Permissions| RoleInfo {
        id: Id::new(60),
        guild_id: Id::new(GUILD),
        name: "mods".into(),
        position: 3,
        permissions,
        color: 0,
        hoist: false,
        mentionable: false,
    };

    harness
        .emit(Event::RoleChanged {
            before: role(Permissions::VIEW_CHANNEL),
            after: role(Permissions::ADD_REACTIONS),
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(
        posted[0].embeds.contains("+ADD_REACTIONS"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("-VIEW_CHANNEL"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn an_overwrite_change_names_the_moderator_whose_entry_has_the_same_channel_and_target() {
    let harness = harness();
    harness.api().add_audit_entry(overwrite_audit(
        AuditLogEventType::ChannelOverwriteCreate,
        CHANNEL,
        9,
        5,
    ));
    let mut after = channel_info("general", None);
    after.overwrites = vec![role_overwrite(
        9,
        Permissions::VIEW_CHANNEL,
        Permissions::empty(),
    )];

    harness
        .emit(Event::ChannelChanged {
            before: channel_info("general", None),
            after,
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(posted[0].embeds.contains("<@5>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn an_overwrite_change_of_another_channel_is_not_named() {
    let harness = harness();
    harness.api().add_audit_entry(overwrite_audit(
        AuditLogEventType::ChannelOverwriteCreate,
        CHANNEL + 1,
        9,
        5,
    ));
    let mut after = channel_info("general", None);
    after.overwrites = vec![role_overwrite(
        9,
        Permissions::VIEW_CHANNEL,
        Permissions::empty(),
    )];

    harness
        .emit(Event::ChannelChanged {
            before: channel_info("general", None),
            after,
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(!posted[0].embeds.contains("<@5>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_channel_change_shows_the_old_and_new_topic_in_code_blocks() {
    let harness = harness();

    harness
        .emit(Event::ChannelChanged {
            before: channel_info("general", Some("old topic")),
            after: channel_info("general", Some("new topic")),
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("Channel changed"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("```old topic```"),
        "{}",
        posted[0].embeds
    );
    assert!(
        posted[0].embeds.contains("```new topic```"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn a_change_that_shows_nothing_is_not_posted() {
    let harness = harness();

    harness
        .emit(Event::ChannelChanged {
            before: channel_info("general", None),
            after: channel_info("general", None),
        })
        .await
        .unwrap();

    assert!(sent(&harness).is_empty());
}

#[tokio::test]
async fn a_voice_join_is_logged_only_when_the_voice_setting_is_on() {
    let harness = harness();
    let joined = Event::VoiceStateChanged {
        guild_id: Id::new(GUILD),
        user: member(40),
        before: None,
        after: Some(Id::new(CHANNEL)),
    };

    harness.emit(joined.clone()).await.unwrap();
    assert!(sent(&harness).is_empty(), "voice is off by default");

    let mut value = config(None, true);
    value["voice"] = json!(true);
    harness.set_config(value);
    harness.emit(joined).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("Joined a voice channel"),
        "{}",
        posted[0].embeds
    );
}

#[tokio::test]
async fn a_nickname_change_shows_both_nicknames_in_code_blocks() {
    let harness = harness();

    harness
        .emit(Event::NicknameChanged {
            guild_id: Id::new(GUILD),
            user: member(40),
            before: Some("Old name".into()),
            after: None,
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(
        posted[0].embeds.contains("```Old name```"),
        "{}",
        posted[0].embeds
    );
    assert!(posted[0].embeds.contains("(none)"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_role_change_shows_its_colour_as_a_hex_code() {
    let harness = harness();
    let role = |colour: u32| RoleInfo {
        id: Id::new(60),
        guild_id: Id::new(GUILD),
        name: "mods".into(),
        position: 3,
        permissions: Permissions::empty(),
        color: colour,
        hoist: false,
        mentionable: false,
    };

    harness
        .emit(Event::RoleChanged {
            before: role(0),
            after: role(0x00FF_0000),
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(posted[0].embeds.contains("#FF0000"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn an_emoji_creation_names_the_emoji() {
    let harness = harness();

    harness
        .emit(Event::EmojiCreated(EmojiInfo {
            id: Id::new(70),
            guild_id: Id::new(GUILD),
            name: "wave".into(),
            animated: false,
            available: true,
        }))
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{:?}", posted.len());
    assert!(posted[0].embeds.contains(":wave:"), "{}", posted[0].embeds);
    assert!(
        posted[0].embeds.contains("Emoji created"),
        "{}",
        posted[0].embeds
    );
}

#[test]
fn a_text_with_backticks_cannot_end_its_code_block() {
    assert_eq!(code_block("a```b"), "```a`\u{200B}``b```");
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

/// The snowflakes count from Discord's epoch, in milliseconds.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// An audit log entry of `kind` on `target`, made by `by` with `reason`, `seconds_ago` seconds ago.
fn audit(
    kind: AuditLogEventType,
    target: u64,
    by: u64,
    reason: Option<&str>,
    seconds_ago: u64,
) -> AuditLogEntry {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the Unix epoch")
        .as_millis();
    let made_ms = u64::try_from(now_ms).expect("the time fits in a u64") - seconds_ago * 1000;
    AuditLogEntry {
        action_type: kind,
        changes: Vec::new(),
        guild_id: None,
        id: Id::new((made_ms - DISCORD_EPOCH_MS) << 22),
        options: None,
        reason: reason.map(str::to_owned),
        target_id: Some(Id::new(target)),
        user_id: Some(Id::new(by)),
    }
}

#[tokio::test]
async fn a_departure_the_audit_log_records_as_a_kick_is_shown_as_one_with_who_made_it() {
    let harness = harness();
    harness
        .api()
        .add_audit_entry(audit(AuditLogEventType::MemberKick, 40, 9, Some("spam"), 5));

    harness
        .emit(Event::MemberLeft {
            guild_id: Id::new(GUILD),
            user: member(40),
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(
        posted[0].embeds.contains("Member kicked"),
        "{}",
        posted[0].embeds
    );
    assert!(posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
    assert!(posted[0].embeds.contains("spam"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_departure_with_only_an_old_kick_in_the_audit_log_is_a_leave() {
    let harness = harness();
    harness
        .api()
        .add_audit_entry(audit(AuditLogEventType::MemberKick, 40, 9, None, 3600));

    harness
        .emit(Event::MemberLeft {
            guild_id: Id::new(GUILD),
            user: member(40),
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(
        posted[0].embeds.contains("Member left"),
        "{}",
        posted[0].embeds
    );
    assert!(!posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_ban_names_who_made_it_and_the_reason_they_gave() {
    let harness = harness();
    harness.api().add_audit_entry(audit(
        AuditLogEventType::MemberBanAdd,
        40,
        9,
        Some("raid"),
        5,
    ));

    harness
        .emit(Event::BanAdded {
            guild_id: Id::new(GUILD),
            user: member(40),
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(
        posted[0].embeds.contains("Member banned"),
        "{}",
        posted[0].embeds
    );
    assert!(posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
    assert!(posted[0].embeds.contains("raid"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_nickname_change_names_who_made_it() {
    let harness = harness();
    harness
        .api()
        .add_audit_entry(audit(AuditLogEventType::MemberUpdate, 40, 9, None, 5));

    harness
        .emit(Event::NicknameChanged {
            guild_id: Id::new(GUILD),
            user: member(40),
            before: Some("Old name".into()),
            after: None,
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_departure_is_still_logged_when_the_audit_log_cannot_be_read() {
    let harness = harness();
    harness
        .api()
        .add_audit_entry(audit(AuditLogEventType::MemberKick, 40, 9, None, 5));
    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("missing access".into()));

    harness
        .emit(Event::MemberLeft {
            guild_id: Id::new(GUILD),
            user: member(40),
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(
        posted[0].embeds.contains("Member left"),
        "{}",
        posted[0].embeds
    );
    assert!(!posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_voice_disconnect_names_the_moderator_who_made_it() {
    let harness = harness();
    let mut value = config(None, true);
    value["voice"] = json!(true);
    harness.set_config(value);
    harness
        .api()
        .add_audit_entry(audit(AuditLogEventType::MemberDisconnect, 40, 9, None, 5));

    harness
        .emit(Event::VoiceStateChanged {
            guild_id: Id::new(GUILD),
            user: member(40),
            before: Some(Id::new(CHANNEL)),
            after: None,
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_voice_move_names_the_moderator_who_made_it() {
    let harness = harness();
    let mut value = config(None, true);
    value["voice"] = json!(true);
    harness.set_config(value);
    harness
        .api()
        .add_audit_entry(audit(AuditLogEventType::MemberMove, 40, 9, None, 5));

    harness
        .emit(Event::VoiceStateChanged {
            guild_id: Id::new(GUILD),
            user: member(40),
            before: Some(Id::new(CHANNEL)),
            after: Some(Id::new(OTHER_LOG)),
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

/// An audit log entry of a bulk delete of `count` messages in `channel`, made by `by`.
fn bulk_audit(channel: u64, count: usize, by: u64) -> AuditLogEntry {
    let mut entry = audit(AuditLogEventType::MessageBulkDelete, channel, by, None, 5);
    entry.options = Some(
        serde_json::from_value(
            json!({ "channel_id": channel.to_string(), "count": count.to_string() }),
        )
        .unwrap(),
    );
    entry
}

#[tokio::test]
async fn a_bulk_delete_names_the_moderator_whose_entry_has_the_same_channel_and_count() {
    let harness = harness();
    harness.api().add_audit_entry(bulk_audit(CHANNEL, 2, 9));

    harness
        .emit(Event::MessagesBulkDeleted {
            guild_id: Id::new(GUILD),
            channel_id: Id::new(CHANNEL),
            message_ids: vec![Id::new(22), Id::new(23)],
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_bulk_delete_of_another_count_is_not_named() {
    let harness = harness();
    harness.api().add_audit_entry(bulk_audit(CHANNEL, 3, 9));

    harness
        .emit(Event::MessagesBulkDeleted {
            guild_id: Id::new(GUILD),
            channel_id: Id::new(CHANNEL),
            message_ids: vec![Id::new(22), Id::new(23)],
        })
        .await
        .unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(!posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

/// An audit log entry of a deletion of a message by `author` in `channel`, made by `by`.
fn deletion_audit(author: u64, channel: u64, by: u64) -> AuditLogEntry {
    let mut entry = audit(AuditLogEventType::MessageDelete, author, by, None, 5);
    entry.options =
        Some(serde_json::from_value(json!({ "channel_id": channel.to_string() })).unwrap());
    entry
}

#[tokio::test]
async fn a_message_a_moderator_deleted_names_the_moderator() {
    let harness = harness();
    harness
        .seed_message(saved(16, "消される文章", Vec::new()))
        .await
        .unwrap();
    harness.api().add_audit_entry(deletion_audit(5, CHANNEL, 9));

    harness.emit(deleted(16)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn a_message_its_author_deleted_names_no_one_else() {
    let harness = harness();
    harness
        .seed_message(saved(16, "消される文章", Vec::new()))
        .await
        .unwrap();

    harness.emit(deleted(16)).await.unwrap();

    let posted = sent(&harness);
    assert_eq!(posted.len(), 1, "{}", posted.len());
    assert!(!posted[0].embeds.contains("<@9>"), "{}", posted[0].embeds);
}

#[tokio::test]
async fn an_edit_of_a_bot_message_is_not_logged() {
    let harness = harness();
    let mut event = edited(16, "embed only", Vec::new(), Vec::new());
    if let Event::MessageUpdated(info) = &mut event {
        info.author.bot = true;
    }

    harness.emit(event).await.unwrap();

    assert!(sent(&harness).is_empty(), "{:?}", sent(&harness).len());
}

#[tokio::test]
async fn an_edit_that_leaves_the_text_as_it_was_is_not_logged() {
    let harness = harness();

    harness
        .emit(replacing(
            edited(16, "same text", Vec::new(), Vec::new()),
            "same text",
        ))
        .await
        .unwrap();

    assert!(sent(&harness).is_empty(), "{:?}", sent(&harness).len());
}
