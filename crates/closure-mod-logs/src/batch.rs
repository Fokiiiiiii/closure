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
        let mut all = kept.embeds;
        all.extend(incoming.iter().cloned());
        let size = serde_json::to_vec(&all).map_or(usize::MAX, |bytes| bytes.len());
        if all.len() >= MAX_EMBEDS || size > MAX_BATCH_BYTES {
            // The batch is emptied first, so that the same entries are not posted twice.
            match store
                .replace(&key, &Batch::default(), expect, Some(KEEP))
                .await
            {
                Ok(_) => return Gathered::Post(all),
                Err(StorageError::Conflict) => continue,
                Err(_) => break,
            }
        }
        match store
            .replace(&key, &Batch { embeds: all }, expect, Some(KEEP))
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
        Ok(job) => job.key(key).if_absent().after(FLUSH_DELAY).await,
        Err(error) => Err(error),
    };
    if let Err(error) = scheduled {
        tracing::warn!(%error, "could not schedule the post of a log batch");
    }
}

/// Posts what the batch of a destination holds.
pub async fn flush(cx: JobCx) -> Result<(), ModuleError> {
    let destination: Destination = cx.payload()?;
    let (ctx, key) = (cx.ctx(), key_of(&destination));
    let store = ctx.store();
    for _ in 0..TRIES {
        let Some(entry) = store.entry(&key).await.map_err(|e| stored(&e))? else {
            return Ok(());
        };
        let batch: Batch = serde_json::from_value(entry.value).unwrap_or_default();
        if batch.embeds.is_empty() {
            return Ok(());
        }
        match store
            .replace(
                &key,
                &Batch::default(),
                Expect::Revision(entry.revision),
                Some(KEEP),
            )
            .await
        {
            Ok(_) => return post_or_retry(ctx, destination, entries_of(batch.embeds)).await,
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the log batch kept changing"))
}
