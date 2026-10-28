//! The logs module for closure: what happens to the messages and members of a server, posted to a
//! channel. A deleted message is shown with what the history kept of it, its attachments included.

use closure_core::action::{Action, MAX_EMBEDS, MentionPolicy};
use closure_core::capability::Capability;
use closure_core::error::{CoreError, ModuleError};
use closure_core::event::{
    AttachmentInfo, ChannelId, Event, EventKind, MessageId, MessageInfo, UserInfo,
};
use closure_core::history::SavedMessage;
use closure_core::locale::Locale;
use closure_core::module::EventContext;
use closure_core::subscription::{Flow, Subscription};
use closure_sdk::{Args, Color, EmbedBuilder, ModuleBuilder, SdkModule, parse_module_config};
use serde::Deserialize;
use serde_json::json;
use twilight_model::channel::message::Embed;
use twilight_model::gateway::Intents;
use twilight_model::id::Id;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

/// The module's section of a guild's configuration.
// One switch per kind of entry, as the configuration names them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Deserialize)]
struct Config {
    /// The channel the entries are posted to; `None` takes the guild's log channel.
    channel: Option<String>,
    message_edits: bool,
    message_deletes: bool,
    member_joins: bool,
    member_leaves: bool,
    bans: bool,
}

fn settings() -> (serde_json::Value, serde_json::Value) {
    let schema = json!({
        "type": "object",
        "properties": {
            "channel": {
                "anyOf": [{ "type": "string", "pattern": "^[0-9]+$" }, { "type": "null" }],
                "description": "The channel the entries are posted to. Left out, the guild's log channel.",
                "x-ui": { "level": "basic" }
            },
            "message_edits": { "type": "boolean", "description": "Post the edits of messages." },
            "message_deletes": {
                "type": "boolean",
                "description": "Post the deletions of messages, with their text and attachments as the history kept them.",
                "x-ui": { "level": "basic" }
            },
            "member_joins": { "type": "boolean", "description": "Post the joins of members." },
            "member_leaves": { "type": "boolean", "description": "Post the departures of members." },
            "bans": { "type": "boolean", "description": "Post the bans and unbans of members." }
        },
        "additionalProperties": false
    });
    let defaults = json!({
        "channel": null,
        "message_edits": true,
        "message_deletes": true,
        "member_joins": true,
        "member_leaves": true,
        "bans": true
    });
    (schema, defaults)
}

pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let (schema, defaults) = settings();
    Ok(
        ModuleBuilder::new("logs", "Logs", env!("CARGO_PKG_VERSION"))?
            .description("Post the edits and deletions of messages, and the joins, leaves and bans of members, to a channel.")
            .capability(Capability::SendMessages)
            .capability(Capability::ReadMessageHistory)
            .intents(Intents::GUILD_MESSAGES | Intents::GUILD_MEMBERS | Intents::GUILD_MODERATION)
            .on(Subscription::new(EventKind::MessageUpdated), on_event)
            .on(Subscription::new(EventKind::MessageDeleted), on_event)
            .on(Subscription::new(EventKind::MessagesBulkDeleted), on_event)
            .on(Subscription::new(EventKind::MemberJoined), on_event)
            .on(Subscription::new(EventKind::MemberLeft), on_event)
            .on(Subscription::new(EventKind::BanAdded), on_event)
            .on(Subscription::new(EventKind::BanRemoved), on_event)
            .config(schema, defaults)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

/// Discord takes at most this many characters in one embed field value.
const FIELD_LIMIT: usize = 1024;

/// One entry of the log: its embed, and the links that the text of the post shows, so that Discord
/// previews the pictures of a message that was deleted.
struct Entry {
    embed: Embed,
    links: Vec<String>,
}

async fn on_event(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let config: Config = parse_module_config(&ctx)?;
    let Some(channel) = destination(&ctx, &config) else {
        return Ok(Flow::Continue);
    };
    let entries = match &event {
        Event::MessageUpdated(message) if config.message_edits => vec![edited(&ctx, message)?],
        Event::MessageDeleted {
            channel_id,
            message_id,
            ..
        } if config.message_deletes => vec![deleted(&ctx, *channel_id, *message_id).await?],
        Event::MessagesBulkDeleted {
            channel_id,
            message_ids,
            ..
        } if config.message_deletes => bulk_deleted(&ctx, *channel_id, message_ids).await?,
        Event::MemberJoined(member) if config.member_joins => {
            vec![member_entry(
                &ctx,
                "logs-joined",
                Color::Success,
                &member.user,
            )?]
        }
        Event::MemberLeft { user, .. } if config.member_leaves => {
            vec![member_entry(&ctx, "logs-left", Color::Info, user)?]
        }
        Event::BanAdded { user, .. } if config.bans => {
            vec![member_entry(&ctx, "logs-banned", Color::Error, user)?]
        }
        Event::BanRemoved { user, .. } if config.bans => {
            vec![member_entry(&ctx, "logs-unbanned", Color::Success, user)?]
        }
        _ => return Ok(Flow::Continue),
    };
    post(&ctx, channel, entries).await?;
    Ok(Flow::Continue)
}

/// The channel the entries go to: the one the module is configured with, else the guild's log channel.
fn destination(ctx: &EventContext, config: &Config) -> Option<ChannelId> {
    config
        .channel
        .as_deref()
        .and_then(|text| text.parse::<u64>().ok())
        .and_then(Id::new_checked)
        .or(ctx.policy().log_channel)
}

async fn saved(
    ctx: &EventContext,
    channel: ChannelId,
    message: MessageId,
) -> Result<Option<SavedMessage>, ModuleError> {
    ctx.history()
        .get(channel, message)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))
}

async fn deleted(
    ctx: &EventContext,
    channel: ChannelId,
    message: MessageId,
) -> Result<Entry, ModuleError> {
    match saved(ctx, channel, message).await? {
        Some(kept) => deleted_entry(ctx, &kept),
        None => unkept_entry(ctx, channel.get(), message.get()),
    }
}

/// The deletions of a bulk delete: an entry for each message the history kept, and one summary
/// that counts the messages and the ones whose content was not kept.
async fn bulk_deleted(
    ctx: &EventContext,
    channel: ChannelId,
    messages: &[MessageId],
) -> Result<Vec<Entry>, ModuleError> {
    let mut entries = Vec::new();
    let mut unkept = 0_u64;
    for message in messages {
        match saved(ctx, channel, *message).await? {
            Some(kept) => entries.push(deleted_entry(ctx, &kept)?),
            None => unkept += 1,
        }
    }
    let count = u64::try_from(messages.len()).unwrap_or(u64::MAX);
    let title = ctx.t_in(ctx.locale, "logs-bulk", &Args::new().with("count", count));
    let mut summary = card(title, Color::Error);
    if unkept > 0 {
        let note = ctx.t_in(
            ctx.locale,
            "logs-bulk-unknown",
            &Args::new().with("count", unkept),
        );
        summary = summary.field(label(ctx, "logs-content"), note, false);
    }
    entries.insert(
        0,
        Entry {
            embed: build(summary)?,
            links: Vec::new(),
        },
    );
    Ok(entries)
}

fn deleted_entry(ctx: &EventContext, message: &SavedMessage) -> Result<Entry, ModuleError> {
    message_entry(
        ctx,
        label(ctx, "logs-deleted"),
        Color::Error,
        message.author.get(),
        message.channel_id.get(),
        &message.content,
        &message.attachments,
    )
}

fn edited(ctx: &EventContext, message: &MessageInfo) -> Result<Entry, ModuleError> {
    message_entry(
        ctx,
        label(ctx, "logs-edited"),
        Color::Warning,
        message.author.id.get(),
        message.channel_id.get(),
        &message.content,
        &message.attachments,
    )
}

/// A message with its author, channel, text and attachments. The links of the attachments go into
/// the text of the post, so that Discord shows the pictures.
fn message_entry(
    ctx: &EventContext,
    title: String,
    color: Color,
    author: u64,
    channel: u64,
    text: &str,
    attachments: &[AttachmentInfo],
) -> Result<Entry, ModuleError> {
    let content = if text.is_empty() {
        label(ctx, "logs-no-text")
    } else {
        clip(text, FIELD_LIMIT)
    };
    let mut card = card(title, color)
        .field(
            label(ctx, "logs-author"),
            format!("<@{author}> ({author})"),
            true,
        )
        .field(label(ctx, "logs-channel"), format!("<#{channel}>"), true)
        .field(label(ctx, "logs-content"), content, false);
    if !attachments.is_empty() {
        let files = clip(&attachments_text(attachments), FIELD_LIMIT);
        card = card.field(label(ctx, "logs-attachments"), files, false);
    }
    let links = attachments
        .iter()
        .filter(|file| !file.url.is_empty())
        .map(|file| file.url.clone())
        .collect();
    Ok(Entry {
        embed: build(card)?,
        links,
    })
}

/// A deleted message the history did not keep: the channel and the message id, and no content.
fn unkept_entry(ctx: &EventContext, channel: u64, message: u64) -> Result<Entry, ModuleError> {
    let card = card(label(ctx, "logs-deleted"), Color::Error)
        .field(label(ctx, "logs-channel"), format!("<#{channel}>"), true)
        .field(label(ctx, "logs-message"), message.to_string(), true)
        .field(
            label(ctx, "logs-content"),
            label(ctx, "logs-not-kept"),
            false,
        );
    Ok(Entry {
        embed: build(card)?,
        links: Vec::new(),
    })
}

fn member_entry(
    ctx: &EventContext,
    title: &str,
    color: Color,
    user: &UserInfo,
) -> Result<Entry, ModuleError> {
    let card = card(label(ctx, title), color).field(
        label(ctx, "logs-member"),
        format!("<@{}> ({})", user.id.get(), user.name),
        false,
    );
    Ok(Entry {
        embed: build(card)?,
        links: Vec::new(),
    })
}

fn attachments_text(attachments: &[AttachmentInfo]) -> String {
    attachments
        .iter()
        .map(|file| {
            let size = format!("{} bytes", file.size);
            if file.url.is_empty() {
                format!("{} ({size})", file.filename)
            } else {
                format!("{} ({size}): {}", file.filename, file.url)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cuts `text` to `limit` characters; the last one then marks that the text went on.
fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(limit - 1).collect();
    clipped.push('…');
    clipped
}

fn label(ctx: &EventContext, key: &str) -> String {
    ctx.t_in(ctx.locale, key, &Args::new())
}

fn card(title: String, color: Color) -> EmbedBuilder {
    EmbedBuilder::new().title(title).color(color)
}

fn build(card: EmbedBuilder) -> Result<Embed, ModuleError> {
    card.build()
        .map_err(|error| ModuleError::other(error.to_string()))
}

/// Sends the entries, as many embeds to a message as Discord takes. The links of the entries go
/// into the text, one to a line.
async fn post(
    ctx: &EventContext,
    channel: ChannelId,
    entries: Vec<Entry>,
) -> Result<(), ModuleError> {
    for batch in entries.chunks(MAX_EMBEDS) {
        let embeds = batch.iter().map(|entry| entry.embed.clone()).collect();
        let links: Vec<&str> = batch
            .iter()
            .flat_map(|entry| entry.links.iter().map(String::as_str))
            .collect();
        let content = (!links.is_empty()).then(|| links.join("\n"));
        ctx.api
            .execute(Action::SendMessage {
                channel_id: channel,
                content,
                embeds,
                reply_to: None,
                mentions: MentionPolicy::None,
            })
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use closure_core::action::Action;
    use closure_core::capability::Capability;
    use closure_core::event::{AttachmentInfo, Event, MemberInfo, MessageInfo, UserInfo};
    use closure_core::history::SavedMessage;
    use closure_core::module::Module;
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
            "bans": true
        })
    }

    fn picture(id: u64) -> AttachmentInfo {
        AttachmentInfo {
            id,
            filename: "photo.png".into(),
            size: 2048,
            content_type: Some("image/png".into()),
            url: format!("https://cdn.example/attachments/{id}/photo.png"),
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

    fn edited(id: u64, content: &str, attachments: Vec<AttachmentInfo>) -> Event {
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
            embed_count: 0,
            sticker_count: 0,
            reply_to: None,
            from_webhook: false,
            created_at: 1_700_000_000,
            edited_at: Some(1_700_000_100),
        })
    }

    /// What the module sent: the channel, the text, and the JSON of the embeds.
    fn sent(harness: &Harness) -> Vec<(u64, Option<String>, String)> {
        harness
            .actions()
            .iter()
            .filter_map(|action| match action {
                Action::SendMessage {
                    channel_id,
                    content,
                    embeds,
                    ..
                } => Some((
                    channel_id.get(),
                    content.clone(),
                    serde_json::to_string(embeds).unwrap(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_deleted_image_is_logged_with_its_name_and_its_link() {
        let harness = harness();
        harness
            .seed_message(saved(10, "look at this", vec![picture(1)]))
            .await
            .unwrap();

        harness.emit(deleted(10)).await.unwrap();

        let messages = sent(&harness);
        assert_eq!(messages.len(), 1, "{messages:?}");
        let (channel, content, embeds) = &messages[0];
        assert_eq!(*channel, LOG);
        assert!(embeds.contains("look at this"), "{embeds}");
        assert!(embeds.contains("photo.png"), "{embeds}");
        let link = "https://cdn.example/attachments/1/photo.png";
        assert!(embeds.contains(link), "{embeds}");
        assert_eq!(
            content.as_deref(),
            Some(link),
            "the link goes in the text so Discord previews it"
        );
    }

    #[tokio::test]
    async fn a_deleted_message_the_history_did_not_keep_is_logged_as_not_kept() {
        let harness = harness();

        harness.emit(deleted(11)).await.unwrap();

        let messages = sent(&harness);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].2.contains("The content was not kept"),
            "{messages:?}"
        );
    }

    #[tokio::test]
    async fn a_bulk_deletion_logs_each_kept_message_with_its_image_and_counts_the_rest() {
        let harness = harness();
        harness
            .seed_message(saved(20, "first", vec![picture(2)]))
            .await
            .unwrap();
        harness
            .seed_message(saved(21, "second", Vec::new()))
            .await
            .unwrap();

        harness
            .emit(Event::MessagesBulkDeleted {
                guild_id: Id::new(GUILD),
                channel_id: Id::new(CHANNEL),
                message_ids: vec![Id::new(20), Id::new(21), Id::new(22)],
            })
            .await
            .unwrap();

        let text = sent(&harness)
            .into_iter()
            .map(|(_, content, embeds)| format!("{content:?} {embeds}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("3 messages deleted"), "{text}");
        assert!(text.contains("first") && text.contains("second"), "{text}");
        assert!(
            text.contains("https://cdn.example/attachments/2/photo.png"),
            "{text}"
        );
        assert!(text.contains("1 of them were not kept"), "{text}");
    }

    #[tokio::test]
    async fn deletions_are_not_logged_when_their_setting_is_off() {
        let harness = harness();
        harness.set_config(config(None, false));

        harness.emit(deleted(12)).await.unwrap();

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

        harness.emit(deleted(13)).await.unwrap();

        let messages = sent(&harness);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(messages[0].0, OTHER_LOG);
    }

    #[tokio::test]
    async fn an_edit_shows_the_new_text_and_its_attachments() {
        let harness = harness();

        harness
            .emit(edited(30, "now with a picture", vec![picture(3)]))
            .await
            .unwrap();

        let messages = sent(&harness);
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(messages[0].2.contains("Message edited"), "{messages:?}");
        assert!(messages[0].2.contains("now with a picture"), "{messages:?}");
        assert!(
            messages[0]
                .2
                .contains("https://cdn.example/attachments/3/photo.png"),
            "{messages:?}"
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
            .map(|message| message.2)
            .collect();
        assert_eq!(texts.len(), 4, "{texts:?}");
        assert!(texts[0].contains("Member joined") && texts[0].contains("newcomer"));
        assert!(texts[1].contains("Member left"));
        assert!(texts[2].contains("Member banned"));
        assert!(texts[3].contains("Member unbanned"));
    }

    #[test]
    fn the_module_reads_the_history_it_logs() {
        let module = module().unwrap();

        assert!(
            module
                .manifest()
                .capabilities
                .contains(Capability::ReadMessageHistory)
        );
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
}
