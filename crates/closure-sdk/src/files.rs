//! Posting the files that messages carried, for the logs of modules.
//! Discord serves attachment links only for a while, so this reads the bytes the host kept.

use std::fmt;

use closure_core::action::{Action, FileUpload, MAX_EMBEDS, MentionPolicy, Outcome};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{AttachmentInfo, ChannelId, MessageId};
use closure_core::module::EventContext;
use closure_core::settings::GuildPolicy;
use serde::{Deserialize, Serialize};
use twilight_model::channel::message::Embed;
use twilight_model::channel::message::embed::EmbedImage;

/// Where a log is delivered, with a bot channel to use if the webhook is unavailable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Destination {
    Channel(ChannelId),
    Webhook {
        name: String,
        fallback: Option<ChannelId>,
    },
}

impl Destination {
    #[must_use]
    pub fn from_policy(policy: &GuildPolicy) -> Option<Self> {
        match &policy.log_webhook {
            Some(name) => Some(Self::Webhook {
                name: name.clone(),
                fallback: policy.log_channel,
            }),
            None => policy.log_channel.map(Self::Channel),
        }
    }
}

/// Sends a log message, falling back only when a named webhook is unavailable.
pub async fn send(
    ctx: &EventContext,
    destination: &Destination,
    content: Option<String>,
    embeds: Vec<Embed>,
    file: Option<FileUpload>,
) -> Result<Outcome, DiscordError> {
    let channel_action = |channel_id| match file.clone() {
        Some(file) => Action::SendFile {
            channel_id,
            content: content.clone(),
            embeds: embeds.clone(),
            file,
        },
        None => Action::SendMessage {
            channel_id,
            content: content.clone(),
            embeds: embeds.clone(),
            reply_to: None,
            mentions: MentionPolicy::None,
        },
    };
    match destination {
        Destination::Channel(channel) => ctx.api.execute(channel_action(*channel)).await,
        Destination::Webhook { name, fallback } => {
            let result = ctx
                .api
                .execute(Action::SendViaWebhook {
                    guild_id: ctx.guild_id,
                    name: name.clone(),
                    content: content.clone(),
                    embeds: embeds.clone(),
                    file: file.clone(),
                })
                .await;
            match (result, fallback) {
                (
                    Err(DiscordError::WebhookNotRegistered | DiscordError::WebhookGone),
                    Some(channel),
                ) => {
                    tracing::warn!("webhook unavailable; posting log to fallback channel");
                    ctx.api.execute(channel_action(*channel)).await
                }
                (other, _) => other,
            }
        }
    }
}

/// The most characters Discord takes in the embeds of one message, all of them together.
const MAX_EMBED_CHARS: usize = 6000;

/// The characters of an embed that count toward [`MAX_EMBED_CHARS`]: the title, the
/// description, the author's name, the footer's text, and each field's name and value.
fn embed_chars(embed: &Embed) -> usize {
    let len = |text: &str| text.chars().count();
    embed.title.as_deref().map_or(0, len)
        + embed.description.as_deref().map_or(0, len)
        + embed.author.as_ref().map_or(0, |author| len(&author.name))
        + embed.footer.as_ref().map_or(0, |footer| len(&footer.text))
        + embed
            .fields
            .iter()
            .map(|field| len(&field.name) + len(&field.value))
            .sum::<usize>()
}

/// One file a message carried, as the history has it.
#[derive(Clone, PartialEq, Eq)]
pub struct Kept {
    /// The attachment's id on Discord, which tells the files of a message apart.
    pub id: u64,
    /// The name the file was uploaded with.
    pub filename: String,
    /// The bytes, when the host kept them; `None` when the file was not kept or could not be fetched.
    pub bytes: Option<Vec<u8>>,
}

impl Kept {
    /// The file as a message can carry it, when its bytes are there.
    #[must_use]
    pub fn upload(&self) -> Option<FileUpload> {
        self.bytes.as_ref().map(|data| FileUpload {
            filename: self.filename.clone(),
            data: data.clone(),
        })
    }
}

impl fmt::Debug for Kept {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kept")
            .field("id", &self.id)
            .field("filename", &self.filename)
            .field("bytes", &self.bytes.as_ref().map(Vec::len))
            .finish()
    }
}

/// The files a message carried that the history has, in the order they were kept.
pub async fn kept_files(ctx: &EventContext, message: MessageId) -> Result<Vec<Kept>, ModuleError> {
    let history = ctx.history();
    let mut kept = Vec::new();
    for file in history
        .files_of(message)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?
    {
        let bytes = if file.stored {
            history
                .file_data(message, file.attachment.id)
                .await
                .map_err(|error| ModuleError::other(error.to_string()))?
        } else {
            None
        };
        kept.push(Kept {
            id: file.attachment.id,
            filename: file.attachment.filename,
            bytes,
        });
    }
    Ok(kept)
}

/// The kept bytes of the given attachments of a message, in order; `None` for one not kept.
pub async fn files_of_attachments(
    ctx: &EventContext,
    message: MessageId,
    attachments: &[AttachmentInfo],
) -> Result<Vec<Kept>, ModuleError> {
    let history = ctx.history();
    let mut kept = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let bytes = history
            .file_data(message, attachment.id)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
        kept.push(Kept {
            id: attachment.id,
            filename: attachment.filename.clone(),
            bytes,
        });
    }
    Ok(kept)
}

/// One entry of a log: its embed, and the files it describes. Each file is posted with the embed.
#[derive(Debug)]
pub struct LogEntry {
    pub embed: Embed,
    pub files: Vec<FileUpload>,
}

/// Posts entries in order, packing embeds into as few messages as Discord and `MAX_EMBED_CHARS` allow.
/// message of its own, with the embed of its entry, which shows a picture inside it.
pub async fn post(
    ctx: &EventContext,
    destination: &Destination,
    entries: Vec<LogEntry>,
) -> Result<(), ModuleError> {
    let mut pending: Vec<Embed> = Vec::new();
    for entry in entries {
        if entry.files.is_empty() {
            let chars = embed_chars(&entry.embed);
            if pending.iter().map(embed_chars).sum::<usize>() + chars > MAX_EMBED_CHARS {
                flush(ctx, destination, &mut pending).await?;
            }
            pending.push(entry.embed);
            if pending.len() == MAX_EMBEDS {
                flush(ctx, destination, &mut pending).await?;
            }
            continue;
        }
        flush(ctx, destination, &mut pending).await?;
        for file in entry.files {
            let embeds = vec![embed_with_file(&entry.embed, &file)];
            send(ctx, destination, None, embeds, Some(file)).await?;
        }
    }
    flush(ctx, destination, &mut pending).await
}

/// The extensions of the files that an embed shows as a picture.
const PICTURE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// The embed of an entry, with the file's picture inside it when the file is a picture: the embed
/// names the file that the message carries.
fn embed_with_file(embed: &Embed, file: &FileUpload) -> Embed {
    let mut embed = embed.clone();
    if is_picture(&file.filename) {
        embed.image = Some(EmbedImage {
            height: None,
            proxy_url: None,
            url: format!("attachment://{}", file.filename),
            width: None,
            content_type: None,
            placeholder: None,
            placeholder_version: None,
            flags: None,
        });
    }
    embed
}

fn is_picture(filename: &str) -> bool {
    filename.rsplit_once('.').is_some_and(|(_, extension)| {
        PICTURE_EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

async fn flush(
    ctx: &EventContext,
    destination: &Destination,
    pending: &mut Vec<Embed>,
) -> Result<(), ModuleError> {
    if pending.is_empty() {
        return Ok(());
    }
    send(ctx, destination, None, std::mem::take(pending), None).await?;
    Ok(())
}
