//! Which guild each live interaction belongs to.
//!
//! A module is one object shared by every guild, so it can hold on to the client it was given
//! while handling a guild A event and use it with an interaction token it received in guild B.
//! Discord would accept that: the token alone decides where the response goes. The host
//! therefore records, when an interaction arrives, which guild it came from, and
//! [`ApiClient`](crate::client::ApiClient) answers only interactions of the guild it is bound to.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::event::{GuildId, InteractionToken};

/// How long Discord honours an interaction token (15 minutes), plus a little slack.
const LIFETIME: Duration = Duration::from_secs(16 * 60);

/// More live interactions than this cannot happen in practice. Past it new ones are not
/// recorded, so their responses are refused instead of the memory growing.
const CAPACITY: usize = 100_000;

/// What happened to a token handed to [`InteractionGuilds::register`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recorded {
    New,
    /// The token was recorded before and still is; the first record is kept.
    Known,
    /// The table is full.
    Refused,
}

#[derive(Default)]
struct Records {
    /// Every token is in here and in `by_expiry` exactly once.
    guilds: HashMap<Arc<str>, (GuildId, Instant)>,
    /// Tokens in the order they were recorded. Records are made while holding the lock and
    /// every record lives equally long, so this is also the order in which they expire, and only
    /// the expired front ever has to be looked at.
    by_expiry: VecDeque<(Instant, Arc<str>)>,
    /// How many entries of `by_expiry` were looked at while forgetting expired records.
    examined: usize,
}

impl Records {
    fn forget_expired(&mut self, now: Instant) {
        while let Some((expires, token)) = self.by_expiry.front() {
            self.examined += 1;
            if *expires > now {
                break;
            }
            self.guilds.remove(token);
            self.by_expiry.pop_front();
        }
    }

    fn record(&mut self, token: &str, guild: GuildId, now: Instant) -> Recorded {
        self.forget_expired(now);
        // A token never changes guild: re-recording it neither moves it nor extends it, and
        // cannot make either structure grow.
        if self.guilds.contains_key(token) {
            return Recorded::Known;
        }
        if self.guilds.len() >= CAPACITY {
            return Recorded::Refused;
        }
        let token: Arc<str> = Arc::from(token);
        let expires = now + LIFETIME;
        self.guilds.insert(Arc::clone(&token), (guild, expires));
        self.by_expiry.push_back((expires, token));
        Recorded::New
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
    /// every interaction it receives, before any module sees it. A token that is already
    /// recorded keeps its first record.
    pub fn register(&self, token: &InteractionToken, guild: GuildId) {
        // The time is read under the lock, so records are made in the order of their times.
        let recorded = {
            let mut records = self.lock();
            records.record(token.expose(), guild, Instant::now())
        };
        if recorded == Recorded::Refused {
            // Outside the lock: logging may block.
            tracing::warn!("too many live interactions; not recording this one");
        }
    }

    /// The guild `token` was issued for, if it is known and has not expired.
    #[must_use]
    pub fn guild_of(&self, token: &InteractionToken) -> Option<GuildId> {
        self.guild_of_at(token, Instant::now())
    }

    fn guild_of_at(&self, token: &InteractionToken, now: Instant) -> Option<GuildId> {
        self.lock()
            .guilds
            .get(token.expose())
            .filter(|(_, expires)| *expires > now)
            .map(|(guild, _)| *guild)
    }

    fn lock(&self) -> MutexGuard<'_, Records> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use twilight_model::id::Id;

    use super::*;

    impl InteractionGuilds {
        /// Records at a chosen time, for the tests that need to move it.
        fn register_at(&self, token: &str, guild: u64, now: Instant) -> Recorded {
            self.lock().record(token, Id::new(guild), now)
        }

        fn at(&self, token: &str, now: Instant) -> Option<GuildId> {
            self.guild_of_at(&InteractionToken::new(token), now)
        }
    }

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
        let start = Instant::now();
        guilds.register_at("token", 1, start);

        assert_eq!(guilds.at("token", start + LIFETIME / 2), Some(Id::new(1)));
        assert_eq!(guilds.at("token", start + LIFETIME), None);
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
            assert_eq!(guilds.register_at(&format!("t{n}"), 1, at), Recorded::New);
        }
        guilds
    }

    #[test]
    fn a_full_table_refuses_new_tokens_and_keeps_the_recorded_ones() {
        let start = Instant::now();
        let guilds = full(start);
        let late = start + Duration::from_secs(60);

        assert_eq!(
            guilds.register_at("one-too-many", 2, late),
            Recorded::Refused
        );

        assert_eq!(guilds.at("one-too-many", late), None);
        assert_eq!(guilds.at("t0", late), Some(Id::new(1)));
        assert_eq!(
            guilds.at(&format!("t{}", CAPACITY - 1), late),
            Some(Id::new(1))
        );
    }

    #[test]
    fn recording_resumes_once_tokens_expire() {
        let start = Instant::now();
        let guilds = full(start);
        let expired = start + LIFETIME;

        assert_eq!(guilds.register_at("fresh", 2, expired), Recorded::New);

        assert_eq!(guilds.at("fresh", expired), Some(Id::new(2)));
        assert_eq!(guilds.at("t0", expired), None);
        assert_eq!(guilds.lock().guilds.len(), 1, "the expired ones are gone");
        assert_eq!(guilds.lock().by_expiry.len(), 1);
    }

    #[test]
    fn refusing_tokens_of_a_full_table_looks_at_one_entry_each() {
        let start = Instant::now();
        let guilds = full(start);
        let examined_before = guilds.lock().examined;

        // Nothing has expired, so every refusal stops at the first entry of the queue.
        for n in 0..1000 {
            let at = start + Duration::from_secs(1);
            assert_eq!(
                guilds.register_at(&format!("extra{n}"), 2, at),
                Recorded::Refused
            );
        }

        assert_eq!(guilds.lock().examined - examined_before, 1000);
        assert_eq!(guilds.lock().by_expiry.len(), CAPACITY);
        assert_eq!(guilds.lock().guilds.len(), CAPACITY);
    }

    #[test]
    fn recording_a_token_again_changes_nothing_and_grows_nothing() {
        let guilds = InteractionGuilds::default();
        let start = Instant::now();
        assert_eq!(guilds.register_at("token", 1, start), Recorded::New);

        // More than the capacity, later, and for another guild.
        let later = start + Duration::from_secs(60);
        for _ in 0..=CAPACITY {
            assert_eq!(guilds.register_at("token", 2, later), Recorded::Known);
        }

        assert_eq!(guilds.at("token", later), Some(Id::new(1)), "first record");
        assert_eq!(guilds.lock().guilds.len(), 1);
        assert_eq!(guilds.lock().by_expiry.len(), 1);
        // And it does not live longer than its first record says.
        assert_eq!(guilds.at("token", start + LIFETIME), None);
    }

    #[test]
    fn records_made_from_many_threads_stay_in_expiry_order() {
        let guilds = Arc::new(InteractionGuilds::default());
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let guilds = Arc::clone(&guilds);
                std::thread::spawn(move || {
                    for n in 0..2000 {
                        guilds
                            .register(&InteractionToken::new(format!("{thread}-{n}")), Id::new(1));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }

        let records = guilds.lock();
        assert_eq!(records.by_expiry.len(), 16_000);
        assert!(
            records
                .by_expiry
                .iter()
                .zip(records.by_expiry.iter().skip(1))
                .all(|(earlier, later)| earlier.0 <= later.0),
            "the queue must be sorted by expiry for the expired front to be enough"
        );
    }
}
