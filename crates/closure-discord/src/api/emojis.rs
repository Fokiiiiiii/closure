use closure_core::action::{Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::GuildId;
use twilight_http::request::AuditLogReason;
use twilight_model::id::Id;
use twilight_model::id::marker::EmojiMarker;

use super::{TwilightApi, read, read_list, run};

impl TwilightApi {
    pub(super) async fn list_emojis(&self, guild: GuildId) -> Result<Outcome, DiscordError> {
        read_list(self.http.emojis(guild))
            .await
            .map(Outcome::Emojis)
    }

    pub(super) async fn create_emoji(
        &self,
        guild: GuildId,
        name: &str,
        image: &str,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        read(
            self.http
                .create_emoji(guild, name, image)
                .reason(reason.as_str()),
        )
        .await
        .map(|emoji| Outcome::Emoji(Box::new(emoji)))
    }

    pub(super) async fn delete_emoji(
        &self,
        guild: GuildId,
        emoji: Id<EmojiMarker>,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        run(self.http.delete_emoji(guild, emoji).reason(reason.as_str())).await
    }
}
