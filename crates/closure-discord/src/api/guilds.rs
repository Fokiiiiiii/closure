use std::borrow::Cow;

use closure_core::action::{GuildPatch, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::event::GuildId;
use twilight_http::Client;
use twilight_http::request::AuditLogReason;

use super::{TwilightApi, read, run};
use crate::error::map_http_error;

/// The feature that stops new invites from being made.
const INVITES_DISABLED: &str = "INVITES_DISABLED";

impl TwilightApi {
    pub(super) async fn edit_guild(
        &self,
        guild: GuildId,
        patch: &GuildPatch,
        reason: &Reason,
    ) -> Result<Outcome, DiscordError> {
        // Discord sets the whole list of features, so the invites feature is added to or taken
        // from the list it has now.
        let features = match patch.invites_disabled {
            Some(disabled) => Some(self.features_with_invites(guild, disabled).await?),
            None => None,
        };
        let feature_refs: Option<Vec<&str>> = features
            .as_ref()
            .map(|list| list.iter().map(String::as_str).collect());
        let mut request = self.http.update_guild(guild).reason(reason.as_str());
        if let Some(name) = &patch.name {
            request = request.name(name);
        }
        if let Some(level) = patch.verification_level {
            request = request.verification_level(Some(level));
        }
        if let Some(refs) = &feature_refs {
            request = request.features(refs);
        }
        // Discord answers with a partial guild; success is what the module needs.
        run(request).await
    }

    /// The guild's features, with `INVITES_DISABLED` set or cleared and the others kept.
    async fn features_with_invites(
        &self,
        guild: GuildId,
        disabled: bool,
    ) -> Result<Vec<String>, DiscordError> {
        let current = read(self.http.guild(guild)).await?;
        let mut features: Vec<String> = current
            .features
            .into_iter()
            .map(|feature| Cow::<str>::from(feature).into_owned())
            .filter(|feature| feature != INVITES_DISABLED)
            .collect();
        if disabled {
            features.push(INVITES_DISABLED.to_owned());
        }
        Ok(features)
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
