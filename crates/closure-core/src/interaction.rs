//! Which guild each live interaction belongs to.
//!
//! A module is one object shared by every guild, so it can hold on to the client it was given
//! while handling a guild A event and use it with an interaction token it received in guild B.
//! Discord would accept that: the token alone decides where the response goes. The host
//! therefore records, when an interaction arrives, which guild it came from, and
//! [`ApiClient`](crate::client::ApiClient) answers only interactions of the guild it is bound to.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::event::{GuildId, InteractionToken};

/// How long Discord honours an interaction token (15 minutes), plus a little slack.
const LIFETIME: Duration = Duration::from_secs(16 * 60);

/// More live interactions than this cannot happen in practice. Past it new ones are not
/// recorded, so their responses are refused instead of the memory growing.
const CAPACITY: usize = 100_000;

#[derive(Default)]
struct Records {
    guilds: HashMap<Arc<str>, (GuildId, Instant)>,
    /// Tokens in the order they were recorded. Every record lives equally long, so this is also
    /// the order in which they expire, and only the expired front ever has to be looked at.
    by_expiry: VecDeque<(Instant, Arc<str>)>,
}

impl Records {
    fn forget_expired(&mut self, now: Instant) {
        while let Some((expires, token)) = self.by_expiry.front() {
            if *expires > now {
                break;
            }
            // A token recorded twice keeps its later expiry.
            if self
                .guilds
                .get(token)
                .is_some_and(|(_, latest)| latest == expires)
            {
                self.guilds.remove(token);
            }
            self.by_expiry.pop_front();
        }
    }
}

/// The guild of every interaction token that is still valid. Cheap to share behind an `Arc`.
#[derive(Default)]
pub struct InteractionGuilds {
    records: Mutex<Records>,
}

impl fmt::Debug for InteractionGuilds {
    // Never print the tokens.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractionGuilds")
            .field("len", &self.lock().guilds.len())
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
        let mut records = self.lock();
        records.forget_expired(now);
        if records.guilds.len() >= CAPACITY {
            tracing::error!("too many live interactions; not recording this one");
            return;
        }
        let token: Arc<str> = Arc::from(token.expose());
        let expires = now + LIFETIME;
        records.guilds.insert(Arc::clone(&token), (guild, expires));
        records.by_expiry.push_back((expires, token));
    }

    fn guild_of_at(&self, token: &InteractionToken, now: Instant) -> Option<GuildId> {
        self.lock()
            .guilds
            .get(token.expose())
            .filter(|(_, expires)| *expires > now)
            .map(|(guild, _)| *guild)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Records> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
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

    /// Fills the table with `CAPACITY` tokens recorded at `at`.
    fn full(at: Instant) -> InteractionGuilds {
        let guilds = InteractionGuilds::default();
        for n in 0..CAPACITY {
            guilds.register_at(&InteractionToken::new(format!("t{n}")), Id::new(1), at);
        }
        guilds
    }

    #[test]
    fn a_full_table_refuses_new_tokens_and_keeps_the_recorded_ones() {
        let start = Instant::now();
        let guilds = full(start);
        let late = start + Duration::from_secs(60);

        guilds.register_at(&InteractionToken::new("one-too-many"), Id::new(2), late);

        assert_eq!(
            guilds.guild_of_at(&InteractionToken::new("one-too-many"), late),
            None
        );
        assert_eq!(
            guilds.guild_of_at(&InteractionToken::new("t0"), late),
            Some(Id::new(1))
        );
        assert_eq!(
            guilds.guild_of_at(&InteractionToken::new(format!("t{}", CAPACITY - 1)), late),
            Some(Id::new(1))
        );
    }

    #[test]
    fn recording_resumes_once_tokens_expire() {
        let start = Instant::now();
        let guilds = full(start);
        let expired = start + LIFETIME;

        guilds.register_at(&InteractionToken::new("fresh"), Id::new(2), expired);

        assert_eq!(
            guilds.guild_of_at(&InteractionToken::new("fresh"), expired),
            Some(Id::new(2))
        );
        assert_eq!(
            guilds.guild_of_at(&InteractionToken::new("t0"), expired),
            None
        );
        assert_eq!(guilds.lock().guilds.len(), 1, "the expired ones are gone");
    }

    #[test]
    fn a_full_table_is_not_scanned_again_for_every_refused_token() {
        let start = Instant::now();
        let guilds = full(start);

        // Nothing has expired, so a refused registration must only look at the front.
        let before = guilds.lock().by_expiry.len();
        for n in 0..1000 {
            guilds.register_at(
                &InteractionToken::new(format!("extra{n}")),
                Id::new(2),
                start + Duration::from_secs(1),
            );
        }
        assert_eq!(guilds.lock().by_expiry.len(), before);
        assert_eq!(guilds.lock().guilds.len(), CAPACITY);
    }

    #[test]
    fn a_token_recorded_twice_lives_until_its_later_expiry() {
        let guilds = InteractionGuilds::default();
        let token = InteractionToken::new("token");
        let start = Instant::now();
        let later = start + Duration::from_secs(60);
        guilds.register_at(&token, Id::new(1), start);
        guilds.register_at(&token, Id::new(1), later);

        // The first record has expired, the second has not.
        let between = start + LIFETIME + Duration::from_secs(1);
        guilds.register_at(&InteractionToken::new("other"), Id::new(2), between);

        assert_eq!(guilds.guild_of_at(&token, between), Some(Id::new(1)));
    }
}
