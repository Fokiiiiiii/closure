//! `/clear`: deletes the newest messages of the channel it is used in.

use closure_sdk::duration::snowflake_time;
use closure_sdk::{Args, CommandArgs, CommandCx, Reason, Tone, UserError};

use crate::DAY;

/// The most messages one `/clear` deletes: all that one bulk delete takes.
const MAX_COUNT: u16 = 100;

/// Discord deletes messages in bulk only when they are newer than this many seconds.
const BULK_DELETE_WINDOW: i64 = 14 * DAY;

/// `/clear`: how many of the newest messages to delete.
#[derive(CommandArgs)]
pub(crate) struct Clear {
    /// How many messages, from 1 to 100
    count: i64,
}

/// `/clear`: deletes the newest messages of the channel, whoever wrote them. A message older than
/// fourteen days cannot be deleted in bulk, so it is left, and the reply counts it.
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
    let channel = cx.invocation().channel_id;
    let now = cx.now();
    let newest = cx.api().get_messages(channel, None, limit).await?;
    let (fresh, old): (Vec<_>, Vec<_>) = newest
        .iter()
        .map(|message| message.id)
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
