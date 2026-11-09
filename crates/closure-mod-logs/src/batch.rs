//! Gathers the log entries of a busy server into fewer messages.
//!
//! Discord lets a bot make 50 requests a second in all, and a raid is the time the log is wanted
//! most and the requests are scarcest. A server posts its first few entries a minute as they come.
//! Past that, the entries are kept (in the module's storage, so a restart does not lose them) and
//! posted together: ten embeds go into one message, and a batch that is not full is posted five
//! seconds after its first entry. No entry is left out; it is posted a little later, with others.

use std::time::Duration;

use closure_core::action::MAX_EMBEDS;
use closure_core::error::ModuleError;
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError};
use closure_sdk::JobCx;
use closure_sdk::files::{Destination, LogEntry};
use serde::{Deserialize, Serialize};
use twilight_model::channel::message::Embed;

use crate::post_or_retry;

pub const FLUSH_JOB: &str = "flush_logs";
/// Posts a server makes in a minute before the entries start to be gathered.
const FREE_POSTS: i64 = 6;
const WINDOW: Duration = Duration::from_secs(60);
/// How long a batch that is not full waits for more entries.
const FLUSH_DELAY: Duration = Duration::from_secs(5);
/// A batch is posted before it gets near the size limit of a stored value.
const MAX_BATCH_BYTES: usize = 24_000;
/// A batch that nothing flushed is dropped after this, so storage cannot fill up with it.
const KEEP: Duration = Duration::from_hours(6);
/// How often a change that lost a race to another one is tried again.
const TRIES: usize = 5;
const COUNTER: &str = "log-posts";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Batch {
    embeds: Vec<Embed>,
    /// The first this many embeds are being posted: they stay until the post is known to be
    /// made, so that a stop in between posts them again instead of losing them.
    #[serde(default)]
    claimed: usize,
}

fn key_of(destination: &Destination) -> String {
    match destination {
        Destination::Channel(channel) => format!("batch:c:{channel}"),
        Destination::Webhook { name, .. } => format!("batch:w:{name}"),
    }
}

fn entries_of(embeds: Vec<Embed>) -> Vec<LogEntry> {
    embeds
        .into_iter()
        .map(|embed| LogEntry {
            embed,
            files: Vec::new(),
        })
        .collect()
}

fn stored(error: &StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

/// Counts this post, and says whether the server has made more than its free posts lately.
async fn busy(ctx: &EventContext) -> bool {
    let (store, guild) = (ctx.store(), ctx.guild_id.get());
    if store.counter_add(COUNTER, guild, 1).await.is_err() {
        return false;
    }
    store
        .counter_recent(COUNTER, guild, WINDOW)
        .await
        .is_ok_and(|posts| posts > FREE_POSTS)
}

/// Posts the entries, or keeps them for a batch when the server is busy. An entry with a file is
/// always posted at once: a file does not go into a batch.
pub async fn submit(
    ctx: &EventContext,
    destination: Destination,
    entries: Vec<LogEntry>,
) -> Result<(), ModuleError> {
    if entries.is_empty() || entries.iter().any(|entry| !entry.files.is_empty()) || !busy(ctx).await
    {
        return post_or_retry(ctx, destination, entries).await;
    }
    let embeds: Vec<Embed> = entries.into_iter().map(|entry| entry.embed).collect();
    match gather(ctx, &destination, embeds).await {
        Gathered::Kept => Ok(()),
        Gathered::Post(embeds) => post_or_retry(ctx, destination, entries_of(embeds)).await,
    }
}

enum Gathered {
    /// The entries wait in a batch.
    Kept,
    /// These entries are to be posted now: the batch they joined is full, or the storage failed.
    Post(Vec<Embed>),
}

async fn gather(ctx: &EventContext, destination: &Destination, incoming: Vec<Embed>) -> Gathered {
    let (store, key) = (ctx.store(), key_of(destination));
    for _ in 0..TRIES {
        let Ok(current) = store.entry(&key).await else {
            break;
        };
        let (kept, expect) = match current {
            Some(entry) => (
                serde_json::from_value::<Batch>(entry.value).unwrap_or_default(),
                Expect::Revision(entry.revision),
            ),
            None => (Batch::default(), Expect::Missing),
        };
        let claimed = kept.claimed.min(kept.embeds.len());
        let mut all = kept.embeds;
        all.extend(incoming.iter().cloned());
        let size = serde_json::to_vec(&all).map_or(usize::MAX, |bytes| bytes.len());
        if all.len() - claimed >= MAX_EMBEDS || size > MAX_BATCH_BYTES {
            // What is posted now leaves the batch first, so that the same entries are not posted
            // twice; the part that a flush is posting stays for the flush to take off.
            let posting = all.split_off(claimed);
            match store
                .replace(
                    &key,
                    &Batch {
                        embeds: all,
                        claimed,
                    },
                    expect,
                    Some(KEEP),
                )
                .await
            {
                Ok(_) => return Gathered::Post(posting),
                Err(StorageError::Conflict) => continue,
                Err(_) => break,
            }
        }
        match store
            .replace(
                &key,
                &Batch {
                    embeds: all,
                    claimed,
                },
                expect,
                Some(KEEP),
            )
            .await
        {
            Ok(_) => {
                schedule_flush(ctx, destination, &key).await;
                return Gathered::Kept;
            }
            Err(StorageError::Conflict) => {}
            Err(_) => break,
        }
    }
    Gathered::Post(incoming)
}

/// Books the post of a batch that does not fill up. One job a batch is enough.
async fn schedule_flush(ctx: &EventContext, destination: &Destination, key: &str) {
    let scheduled = match ctx.scheduler().job(FLUSH_JOB, destination) {
        Ok(job) => job.key(key).unless_waiting().after(FLUSH_DELAY).await,
        Err(error) => Err(error),
    };
    if let Err(error) = scheduled {
        tracing::warn!(%error, "could not schedule the post of a log batch");
    }
}

/// Posts what the batch of a destination holds. The embeds are marked as being posted first and
/// taken off the batch only after the post, so that a stop in between posts them again.
pub async fn flush(cx: JobCx) -> Result<(), ModuleError> {
    let destination: Destination = cx.payload()?;
    let (ctx, key) = (cx.ctx(), key_of(&destination));
    let store = ctx.store();
    for _ in 0..TRIES {
        let Some(entry) = store.entry(&key).await.map_err(|e| stored(&e))? else {
            return Ok(());
        };
        let mut batch: Batch = serde_json::from_value(entry.value).unwrap_or_default();
        let claimed = if batch.claimed == 0 || batch.claimed > batch.embeds.len() {
            batch.embeds.len()
        } else {
            batch.claimed
        };
        if claimed == 0 {
            return Ok(());
        }
        batch.claimed = claimed;
        let posting = batch.embeds[..claimed].to_vec();
        match store
            .replace(&key, &batch, Expect::Revision(entry.revision), Some(KEEP))
            .await
        {
            Ok(_) => {}
            Err(StorageError::Conflict) => continue,
            Err(error) => return Err(stored(&error)),
        }
        // A post that failed is kept by the retry job already, so it leaves the batch either way.
        let posted = post_or_retry(ctx, destination.clone(), entries_of(posting)).await;
        let left = acknowledge(ctx, &key).await?;
        // What came while this was posted goes out in the same run.
        if posted.is_err() || left == 0 {
            return posted;
        }
    }
    Err(ModuleError::other("the log batch kept changing"))
}

/// Takes the embeds that were posted off the batch; what came meanwhile stays.
async fn acknowledge(ctx: &EventContext, key: &str) -> Result<usize, ModuleError> {
    let store = ctx.store();
    for _ in 0..TRIES {
        let Some(entry) = store.entry(key).await.map_err(|e| stored(&e))? else {
            return Ok(0);
        };
        let mut batch: Batch = serde_json::from_value(entry.value).unwrap_or_default();
        let done = batch.claimed.min(batch.embeds.len());
        batch.embeds.drain(..done);
        batch.claimed = 0;
        match store
            .replace(key, &batch, Expect::Revision(entry.revision), Some(KEEP))
            .await
        {
            Ok(_) => return Ok(batch.embeds.len()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the log batch kept changing"))
}
