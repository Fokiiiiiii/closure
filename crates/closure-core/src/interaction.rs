//! Which guild each live interaction belongs to.
//!
//! A module is one object shared by every guild, so it can hold on to the client it was given
//! while handling a guild A event and use it with an interaction token it received in guild B.
//! Discord would accept that: the token alone decides where the response goes. The host
//! therefore records, when an interaction arrives, which guild it came from, and
//! [`ApiClient`](crate::client::ApiClient) answers only interactions of the guild it is bound to.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::event::{GuildId, InteractionToken};

/// How long Discord honours an interaction token (15 minutes), plus a little slack.
const LIFETIME: Duration = Duration::from_secs(16 * 60);

/// More live interactions than this cannot happen in practice. Past it new ones are not
/// recorded, so their responses are refused instead of the memory growing.
const CAPACITY: usize = 100_000;

/// The guild of every interaction token that is still valid. Cheap to share behind an `Arc`.
#[derive(Default)]
pub struct InteractionGuilds {
    entries: Mutex<HashMap<String, (GuildId, Instant)>>,
}

impl fmt::Debug for InteractionGuilds {
    // Never print the tokens.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractionGuilds")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl InteractionGuilds {
    /// Records that `token` was issued for an interaction in `guild`. Called by the host for
    /// every interaction it receives, before any module sees it.
    pub fn register(&self, token: &InteractionToken, guild: GuildId) {
        self.register_at(token, guild, Instant::now());
    }

    /// The guild `token` was issued for, if it is known and has not expired.
    #[must_use]
    pub fn guild_of(&self, token: &InteractionToken) -> Option<GuildId> {
        self.guild_of_at(token, Instant::now())
    }

    fn register_at(&self, token: &InteractionToken, guild: GuildId, now: Instant) {
        let mut entries = self.lock();
        if entries.len() >= CAPACITY {
            entries.retain(|_, (_, expires)| *expires > now);
            if entries.len() >= CAPACITY {
                tracing::error!("too many live interactions; not recording this one");
                return;
            }
        }
        entries.insert(token.expose().to_owned(), (guild, now + LIFETIME));
    }

    fn guild_of_at(&self, token: &InteractionToken, now: Instant) -> Option<GuildId> {
        self.lock()
            .get(token.expose())
            .filter(|(_, expires)| *expires > now)
            .map(|(guild, _)| *guild)
    }

    fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (GuildId, Instant)>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    #[test]
    fn a_token_belongs_to_the_guild_it_was_registered_for() {
        let guilds = InteractionGuilds::default();
        let (a, b) = (
            InteractionToken::new("token-a"),
            InteractionToken::new("token-b"),
        );
        guilds.register(&a, Id::new(1));
        guilds.register(&b, Id::new(2));

        assert_eq!(guilds.guild_of(&a), Some(Id::new(1)));
        assert_eq!(guilds.guild_of(&b), Some(Id::new(2)));
        assert_eq!(guilds.guild_of(&InteractionToken::new("forged")), None);
    }

    #[test]
    fn a_token_is_forgotten_once_discord_no_longer_honours_it() {
        let guilds = InteractionGuilds::default();
        let token = InteractionToken::new("token");
        let start = Instant::now();
        guilds.register_at(&token, Id::new(1), start);

        assert_eq!(
            guilds.guild_of_at(&token, start + LIFETIME / 2),
            Some(Id::new(1))
        );
        assert_eq!(guilds.guild_of_at(&token, start + LIFETIME), None);
    }

    #[test]
    fn debug_output_never_contains_a_token() {
        let guilds = InteractionGuilds::default();
        guilds.register(&InteractionToken::new("secret-token"), Id::new(1));
        assert!(!format!("{guilds:?}").contains("secret-token"));
    }
}
