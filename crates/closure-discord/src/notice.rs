//! The replies the host gives itself, as embeds like the replies of the modules: a failure, the usage
//! of a text command, a notice that the bot is busy.

use twilight_model::channel::message::Embed;
use twilight_util::builder::embed::EmbedBuilder;

/// Red, for a failure: the colour the SDK gives its errors.
pub(crate) const ERROR: u32 = 0x00F0_4747;
/// Blue-grey, for information: the colour the SDK gives its plain replies.
pub(crate) const INFO: u32 = 0x002B_2D31;
/// Amber, for a reply that asks the user to wait.
pub(crate) const WAIT: u32 = 0x00FA_A61A;

/// How many characters Discord takes in an embed description.
const DESCRIPTION_CHARS: usize = 4_096;

/// The text as an embed of `color`.
pub(crate) fn embed(text: &str, color: u32) -> Embed {
    EmbedBuilder::new()
        .description(text.chars().take(DESCRIPTION_CHARS).collect::<String>())
        .color(color)
        .build()
}
