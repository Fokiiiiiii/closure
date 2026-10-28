//! The files that messages carried, for the logs of modules: what the history kept of them, and posting
//! them with the entries that describe them. Discord serves an attachment's link only for a while, so a
//! log relies on the bytes the host kept (see `closure_core::attachments`).

use std::fmt;

use closure_core::action::{Action, FileUpload, MAX_EMBEDS, MentionPolicy};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, MessageId};
use closure_core::module::EventContext;
use twilight_model::channel::message::Embed;

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

/// One entry of a log: its embed, and the files it describes. Each file is posted with the embed.
#[derive(Debug)]
pub struct LogEntry {
    pub embed: Embed,
    pub files: Vec<FileUpload>,
}

/// Posts the entries to `channel`, in order. Entries without files share messages, as many embeds to a
/// message as Discord takes and as many characters as `MAX_EMBED_CHARS` allows; a file goes in a
/// message of its own, with the embed of its entry.
pub async fn post(
    ctx: &EventContext,
    channel: ChannelId,
    entries: Vec<LogEntry>,
) -> Result<(), ModuleError> {
    let mut pending: Vec<Embed> = Vec::new();
    for entry in entries {
        if entry.files.is_empty() {
            let chars = embed_chars(&entry.embed);
            if pending.iter().map(embed_chars).sum::<usize>() + chars > MAX_EMBED_CHARS {
                flush(ctx, channel, &mut pending).await?;
            }
            pending.push(entry.embed);
            if pending.len() == MAX_EMBEDS {
                flush(ctx, channel, &mut pending).await?;
            }
            continue;
        }
        flush(ctx, channel, &mut pending).await?;
        for file in entry.files {
            send(
                ctx,
                Action::SendFile {
                    channel_id: channel,
                    content: None,
                    embeds: vec![entry.embed.clone()],
                    file,
                },
            )
            .await?;
        }
    }
    flush(ctx, channel, &mut pending).await
}

async fn flush(
    ctx: &EventContext,
    channel: ChannelId,
    pending: &mut Vec<Embed>,
) -> Result<(), ModuleError> {
    if pending.is_empty() {
        return Ok(());
    }
    send(
        ctx,
        Action::SendMessage {
            channel_id: channel,
            content: None,
            embeds: std::mem::take(pending),
            reply_to: None,
            mentions: MentionPolicy::None,
        },
    )
    .await
}

async fn send(ctx: &EventContext, action: Action) -> Result<(), ModuleError> {
    ctx.api
        .execute(action)
        .await
        .map(|_| ())
        .map_err(|error| ModuleError::other(error.to_string()))
}
