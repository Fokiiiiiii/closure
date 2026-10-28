//! The files of attachments of messages (see [`crate::history::KeptFile`]). Discord serves an
//! attachment's link for about a day, so the bytes are fetched when a deletion or an edit needs them,
//! and kept as the message arrives when a module asks for it. The fetching itself is the host's
//! business ([`Fetcher`]); what is kept, and the limits, are here.

use std::collections::HashSet;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};

use crate::event::{AttachmentInfo, ChannelId, GuildId, MessageId};
use crate::history::{HistoryBackend, HistoryError};

/// A file larger than this is not kept: a log could not post it in one message anyway.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// The files of one guild keep at most this many bytes; the oldest go first.
pub const GUILD_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// How long one fetch may take. A deletion waits for its fetches, so this stays short.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// How many arrivals are fetched at once in the background.
pub const ARRIVAL_FETCHES: usize = 4;

/// The hosts attachments are served from; nothing else is fetched.
const CDN_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];

/// Fetches the bytes behind an attachment's link.
#[async_trait::async_trait]
pub trait Fetcher: Send + Sync {
    /// The bytes at `url`, at most [`MAX_FILE_BYTES`] of them. A redirect is an error.
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Whether `url` is an HTTPS link to Discord's CDN, the only place attachments are fetched from.
#[must_use]
pub fn is_cdn_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    CDN_HOSTS.contains(&host)
}

/// The bytes of one attachment, or `None` when they are not fetched: the link is not on the CDN, the
/// file is too large, or the fetch fails or takes too long.
async fn fetch_attachment(fetcher: &dyn Fetcher, attachment: &AttachmentInfo) -> Option<Vec<u8>> {
    if attachment.size > MAX_FILE_BYTES || !is_cdn_url(&attachment.url) {
        return None;
    }
    match tokio::time::timeout(FETCH_TIMEOUT, fetcher.fetch(&attachment.url)).await {
        Ok(Ok(data)) if u64::try_from(data.len()).is_ok_and(|len| len <= MAX_FILE_BYTES) => {
            Some(data)
        }
        _ => None,
    }
}

/// Keeps the files of `attachments` that are not stored yet: fetched where they can be, and recorded
/// without bytes where they cannot, so that a log can say so. The fetches run at the same time.
pub async fn keep_missing(
    backend: &dyn HistoryBackend,
    fetcher: &dyn Fetcher,
    guild: GuildId,
    channel: ChannelId,
    message: MessageId,
    attachments: &[AttachmentInfo],
    at: i64,
) -> Result<(), HistoryError> {
    if attachments.is_empty() {
        return Ok(());
    }
    let kept: HashSet<u64> = backend
        .files_of(guild, message)
        .await?
        .iter()
        .filter(|file| file.stored)
        .map(|file| file.attachment.id)
        .collect();
    let mut in_flight: FuturesUnordered<_> = attachments
        .iter()
        .filter(|attachment| !kept.contains(&attachment.id))
        .map(|attachment| async move { (attachment, fetch_attachment(fetcher, attachment).await) })
        .collect();
    while let Some((attachment, data)) = in_flight.next().await {
        backend
            .save_file(guild, channel, message, attachment, data.as_deref(), at)
            .await?;
        if data.is_some() {
            backend.trim_files(guild, GUILD_FILE_BYTES).await?;
        }
    }
    Ok(())
}
