use closure_core::action::{GuildPatch, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::GuildId;
use twilight_http::Client;
use twilight_http::request::AuditLogReason;

use super::{TwilightApi, run};
use crate::error::map_http_error;

impl TwilightApi {
    pub(super) async fn edit_guild(
        &self,
        guild: GuildId,
        patch: &GuildPatch,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        let mut request = self.http.update_guild(guild).reason(reason.as_str());
        if let Some(name) = &patch.name {
            request = request.name(name);
        }
        if let Some(level) = patch.verification_level {
            request = request.verification_level(Some(level));
        }
        // Discord answers with a partial guild; success is what the module needs.
        run(request).await
    }
}

/// Discord's page size limit for the bot's guild list.
const PAGE: u16 = 200;

/// Every guild the bot is currently in, read page by page from Discord.
///
/// Gateway events only report changes while connected, so after downtime this is what tells the
/// bot which guilds it was added to or removed from in the meantime.
pub async fn joined_guilds(http: &Client) -> Result<Vec<GuildId>, DiscordError> {
    let mut guilds = Vec::new();
    loop {
        let request = http.current_user_guilds().limit(PAGE);
        let page = match guilds.last() {
            Some(&last) => request.after(last),
            None => request,
        }
        .await
        .map_err(|error| map_http_error(&error))?
        .models()
        .await
        .map_err(|error| {
            DiscordError::Fatal(format!("could not read Discord's response: {error}"))
        })?;

        let full = page.len() == usize::from(PAGE);
        guilds.extend(page.into_iter().map(|guild| guild.id));
        if !full {
            return Ok(guilds);
        }
    }
}
