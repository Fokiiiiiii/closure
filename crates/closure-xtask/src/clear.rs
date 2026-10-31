//! Deletes the messages the bot posted in a test channel, so that a replay can start over. Only the
//! bot's own messages posted at or after the given time are deleted; older ones are kept.

use anyhow::{Context, Result, bail};
use twilight_http::Client;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, MessageMarker, UserMarker};

use crate::replay::{TOKEN_VARIABLE, token_from_env};

/// The most messages Discord returns in one request.
const PAGE_SIZE: u16 = 100;

/// Deletes the bot's own messages in `channel_id` posted at or after `after` (Unix seconds).
///
/// # Errors
///
/// Fails before deleting anything when the token is missing or the channel cannot be read.
pub fn run(channel_id: u64, after: i64) -> Result<bool> {
    let Some(token) = token_from_env() else {
        bail!("{TOKEN_VARIABLE} is not set (in the environment or in .env)");
    };
    let channel: Id<ChannelMarker> =
        Id::new_checked(channel_id).context("--channel must be a non-zero id")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        // The client starts its rate limiter on the runtime it runs in, so it is made here.
        let client = Client::new(token);
        let me = client.current_user().await?.model().await?.id;
        let targets = own_messages_since(&client, channel, me, after).await?;
        let total = targets.len();
        let mut failures = 0_usize;
        for message in targets {
            if let Err(error) = client.delete_message(channel, message).await {
                failures += 1;
                eprintln!("message {message}: not deleted: {error}");
            }
        }
        println!("deleted {} of {total} message(s)", total - failures);
        Ok::<bool, anyhow::Error>(failures == 0)
    })
}

/// The ids of the messages `author` posted in `channel` at or after `after`, newest first.
async fn own_messages_since(
    client: &Client,
    channel: Id<ChannelMarker>,
    author: Id<UserMarker>,
    after: i64,
) -> Result<Vec<Id<MessageMarker>>> {
    let mut found = Vec::new();
    let mut before = None;
    loop {
        let request = client.channel_messages(channel).limit(PAGE_SIZE);
        let response = match before {
            Some(cursor) => request.before(cursor).await?,
            None => request.await?,
        };
        let page = response.models().await?;
        let Some(oldest) = page.last() else {
            break;
        };
        before = Some(oldest.id);
        found.extend(
            page.iter()
                .filter(|message| {
                    message.author.id == author && message.timestamp.as_secs() >= after
                })
                .map(|message| message.id),
        );
        if oldest.timestamp.as_secs() < after {
            break;
        }
    }
    Ok(found)
}
