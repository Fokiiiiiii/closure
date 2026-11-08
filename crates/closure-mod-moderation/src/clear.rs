//! `/clear`: deletes messages of the channel it is used in: the newest ones, or the newest ones
//! that fit what the moderator asked for (a member, bots, a text, links, files).
//!
//! Pinned messages are left unless the moderator says they are meant too. A message older than
//! fourteen days cannot be deleted in bulk, so it is left, and the reply counts it.

use closure_core::action::MessageCursor;
use closure_core::event::MessageId;
use closure_sdk::duration::snowflake_time;
use closure_sdk::{Args, CommandArgs, CommandCx, Reason, Tone, UserError, UserId};
use twilight_model::channel::Message;

use crate::DAY;

/// The most messages one `/clear` deletes: all that one bulk delete takes.
const MAX_COUNT: u16 = 100;

/// The most messages a `/clear` with a filter looks through to find the ones it deletes.
const MAX_SCAN: usize = 500;

/// Discord lists at most this many messages at once.
const PAGE: u16 = 100;

/// Discord deletes messages in bulk only when they are newer than this many seconds.
const BULK_DELETE_WINDOW: i64 = 14 * DAY;

/// `/clear`: how many messages to delete, and which.
#[derive(CommandArgs)]
pub(crate) struct Clear {
    /// How many messages, from 1 to 100
    count: i64,
    /// Only the messages of this member
    user: Option<UserId>,
    /// Yes: only the messages of bots
    bots: Option<bool>,
    /// Only the messages that contain this text
    #[arg(max_length = 100)]
    contains: Option<String>,
    /// Yes: only the messages with a link or an invite
    links: Option<bool>,
    /// Yes: only the messages with a file
    files: Option<bool>,
    /// Yes: pinned messages are deleted too
    pinned: Option<bool>,
}

/// What a message has to be to be deleted: every part that is set has to hold.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one switch for each filter the moderator can set"
)]
struct Wanted {
    user: Option<UserId>,
    bots: bool,
    contains: Option<String>,
    links: bool,
    files: bool,
    pinned: bool,
}

impl Wanted {
    fn of(args: &Clear) -> Self {
        Self {
            user: args.user,
            bots: args.bots == Some(true),
            contains: args
                .contains
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_lowercase),
            links: args.links == Some(true),
            files: args.files == Some(true),
            pinned: args.pinned == Some(true),
        }
    }

    fn narrows(&self) -> bool {
        self.user.is_some() || self.bots || self.contains.is_some() || self.links || self.files
    }

    fn accepts(&self, message: &Message) -> bool {
        if message.pinned && !self.pinned {
            return false;
        }
        if self.user.is_some_and(|user| message.author.id != user) {
            return false;
        }
        if self.bots && !message.author.bot {
            return false;
        }
        if self.files && message.attachments.is_empty() {
            return false;
        }
        let content = message.content.to_lowercase();
        if self
            .contains
            .as_deref()
            .is_some_and(|text| !content.contains(text))
        {
            return false;
        }
        !self.links || has_link(&content)
    }
}

fn has_link(content: &str) -> bool {
    ["http://", "https://", "discord.gg/", "discord.com/invite/"]
        .iter()
        .any(|mark| content.contains(mark))
}

/// `/clear`: deletes the newest messages of the channel that fit, whoever wrote them when no
/// filter is set.
pub(crate) async fn clear(cx: CommandCx, args: Clear) -> Result<(), UserError> {
    // Acknowledged first: the listing and the deletion may outlast Discord's three seconds.
    cx.defer(true).await?;
    let limit = u16::try_from(args.count)
        .ok()
        .filter(|count| (1..=MAX_COUNT).contains(count));
    let Some(limit) = limit else {
        return cx
            .say_private_t(Tone::Error, "clear-range", &Args::new())
            .await;
    };
    let wanted = Wanted::of(&args);
    let channel = cx.invocation().channel_id;
    let found = find(&cx, &wanted, usize::from(limit)).await?;
    // Read after the listing: a message may pass the fourteen days while the listing is on its way.
    let now = cx.now();
    let (fresh, old): (Vec<MessageId>, Vec<MessageId>) = found
        .into_iter()
        .partition(|id| now - snowflake_time(id.get()) <= BULK_DELETE_WINDOW);
    let reason = Reason::truncated(&cx.t("reason-clear", &Args::new()));
    match fresh.as_slice() {
        [] => {}
        [single] => cx.api().delete_message(channel, *single, reason).await?,
        many => {
            cx.api()
                .bulk_delete_messages(channel, many.to_vec(), reason)
                .await?;
        }
    }
    cx.say_private_t(
        Tone::Success,
        "clear-done",
        &Args::new()
            .with("deleted", i64::try_from(fresh.len()).unwrap_or_default())
            .with("skipped", i64::try_from(old.len()).unwrap_or_default()),
    )
    .await
}

/// The ids of the newest messages that fit, at most `limit`, looking through at most
/// [`MAX_SCAN`] messages when a filter is set (else exactly the newest `limit`).
async fn find(cx: &CommandCx, wanted: &Wanted, limit: usize) -> Result<Vec<MessageId>, UserError> {
    let channel = cx.invocation().channel_id;
    let narrows = wanted.narrows();
    let mut found = Vec::new();
    let mut scanned = 0;
    let mut cursor = None;
    while found.len() < limit && scanned < if narrows { MAX_SCAN } else { limit } {
        let ask = if narrows {
            PAGE
        } else {
            u16::try_from(limit - scanned).unwrap_or(PAGE).min(PAGE)
        };
        let page = cx.api().get_messages(channel, cursor, ask).await?;
        if page.is_empty() {
            break;
        }
        for message in &page {
            scanned += 1;
            if wanted.accepts(message) {
                found.push(message.id);
                if found.len() == limit {
                    break;
                }
            }
        }
        if page.len() < usize::from(ask) {
            break;
        }
        cursor = page.last().map(|message| MessageCursor::Before(message.id));
    }
    Ok(found)
}
