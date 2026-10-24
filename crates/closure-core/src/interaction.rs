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

/// How many interactions may be recorded. A record that cannot be made means the interaction
/// cannot be answered by a module, so the limits are far above what a real workload reaches and
/// exist to keep memory bounded when something floods the bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionLimits {
    /// Live records of all guilds together.
    pub total: usize,
    /// Live records of a single guild, so one guild that is flooded with commands cannot use up
    /// the room of all the others. It takes `total / per_guild` such guilds to do that.
    pub per_guild: usize,
}

impl Default for InteractionLimits {
    fn default() -> Self {
        Self {
            total: 200_000,
            per_guild: 20_000,
        }
    }
}

/// What happened to a token handed to [`InteractionGuilds::register`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recorded {
    New,
    /// The token was recorded before and still is; the first record is kept.
    Known,
    /// The guild is over its limit.
    GuildFull,
    /// All guilds together are over the limit.
    TableFull,
}

impl Recorded {
    fn is_recorded(self) -> bool {
        matches!(self, Self::New | Self::Known)
    }
}

#[derive(Default)]
struct Records {
    /// Every token is in here and in `by_expiry` exactly once.
    guilds: HashMap<Arc<str>, (GuildId, Instant)>,
    /// Tokens in the order they were recorded. Records are made while holding the lock and
    /// every record lives equally long, so this is also the order in which they expire, and only
    /// the expired front ever has to be looked at.
    by_expiry: VecDeque<(Instant, Arc<str>)>,
    /// Live records per guild.
    per_guild: HashMap<GuildId, usize>,
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
            if let Some((guild, _)) = self.guilds.remove(token)
                && let Some(count) = self.per_guild.get_mut(&guild)
            {
                *count -= 1;
                if *count == 0 {
                    self.per_guild.remove(&guild);
                }
            }
            self.by_expiry.pop_front();
        }
    }

    fn record(
        &mut self,
        limits: InteractionLimits,
        token: &str,
        guild: GuildId,
        now: Instant,
    ) -> Recorded {
        self.forget_expired(now);
        // A token never changes guild: re-recording it neither moves it nor extends it, and
        // cannot make either structure grow.
        if self.guilds.contains_key(token) {
            return Recorded::Known;
        }
        if self.guilds.len() >= limits.total {
            return Recorded::TableFull;
        }
        let in_guild = self.per_guild.entry(guild).or_insert(0);
        if *in_guild >= limits.per_guild {
            return Recorded::GuildFull;
        }
        *in_guild += 1;
        let token: Arc<str> = Arc::from(token);
        let expires = now + LIFETIME;
        self.guilds.insert(Arc::clone(&token), (guild, expires));
        self.by_expiry.push_back((expires, token));
        Recorded::New
    }
}

type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// The guild of every interaction token that is still valid. Cheap to share behind an `Arc`.
pub struct InteractionGuilds {
    limits: InteractionLimits,
    clock: Clock,
    records: Mutex<Records>,
}

impl Default for InteractionGuilds {
    fn default() -> Self {
        Self::with_limits(InteractionLimits::default())
    }
}

impl fmt::Debug for InteractionGuilds {
    // Never print the tokens.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractionGuilds")
            .field("limits", &self.limits)
            .field("len", &self.lock().guilds.len())
            .finish_non_exhaustive()
    }
}

impl InteractionGuilds {
    #[must_use]
    pub fn with_limits(limits: InteractionLimits) -> Self {
        Self::with_clock(limits, Instant::now)
    }

    fn with_clock(
        limits: InteractionLimits,
        clock: impl Fn() -> Instant + Send + Sync + 'static,
    ) -> Self {
        Self {
            limits,
            clock: Box::new(clock),
            records: Mutex::new(Records::default()),
        }
    }

    /// Records that `token` was issued for an interaction in `guild`. Called by the host for
    /// every interaction it receives, before any module sees it. A token that is already
    /// recorded keeps its first record.
    ///
    /// Returns whether the token is recorded. When it is not (a limit is reached), no module can
    /// answer the interaction, so the host should not hand it to one.
    #[must_use]
    pub fn register(&self, token: &InteractionToken, guild: GuildId) -> bool {
        let recorded = {
            let mut records = self.lock();
            // The time is read under the lock, so records are made in the order of their times.
            let now = (self.clock)();
            records.record(self.limits, token.expose(), guild, now)
        };
        // Outside the lock: logging may block.
        match recorded {
            Recorded::GuildFull => {
                tracing::warn!(%guild, "too many live interactions in this guild; not recording this one");
            }
            Recorded::TableFull => {
                tracing::warn!("too many live interactions; not recording this one");
            }
            Recorded::New | Recorded::Known => {}
        }
        recorded.is_recorded()
    }

    /// The guild `token` was issued for, if it is known and has not expired.
    #[must_use]
    pub fn guild_of(&self, token: &InteractionToken) -> Option<GuildId> {
        self.guild_of_at(token, (self.clock)())
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use twilight_model::id::Id;

    use super::*;

    const LIMITS: InteractionLimits = InteractionLimits {
        total: 100,
        per_guild: 40,
    };

    impl InteractionGuilds {
        /// Records at a chosen time, for the tests that need to move it.
        fn register_at(&self, token: &str, guild: u64, now: Instant) -> Recorded {
            self.lock().record(self.limits, token, Id::new(guild), now)
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
        assert!(guilds.register(&a, Id::new(1)));
        assert!(guilds.register(&b, Id::new(2)));

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
        assert!(guilds.register(&InteractionToken::new("secret-token"), Id::new(1)));
        assert!(!format!("{guilds:?}").contains("secret-token"));
    }

    /// Fills the whole table with tokens recorded at `at`, spread over enough guilds that no
    /// single guild is full.
    fn full(at: Instant) -> InteractionGuilds {
        let guilds = InteractionGuilds::with_limits(LIMITS);
        for n in 0..LIMITS.total {
            let guild = u64::try_from(n % 10).unwrap() + 1;
            assert_eq!(
                guilds.register_at(&format!("t{n}"), guild, at),
                Recorded::New
            );
        }
        guilds
    }

    #[test]
    fn a_full_table_refuses_new_tokens_and_keeps_the_recorded_ones() {
        let start = Instant::now();
        let guilds = full(start);
        let late = start + Duration::from_secs(60);

        assert_eq!(
            guilds.register_at("one-too-many", 11, late),
            Recorded::TableFull
        );

        assert_eq!(guilds.at("one-too-many", late), None);
        assert_eq!(guilds.at("t0", late), Some(Id::new(1)));
        assert_eq!(
            guilds.at(&format!("t{}", LIMITS.total - 1), late),
            Some(Id::new(10))
        );
    }

    #[test]
    fn one_guild_that_is_flooded_cannot_use_up_the_room_of_the_others() {
        let start = Instant::now();
        let guilds = InteractionGuilds::with_limits(LIMITS);

        for n in 0..LIMITS.per_guild {
            assert_eq!(
                guilds.register_at(&format!("flood{n}"), 1, start),
                Recorded::New
            );
        }
        assert_eq!(
            guilds.register_at("flood-more", 1, start),
            Recorded::GuildFull,
            "the flooded guild is refused"
        );

        assert_eq!(guilds.register_at("calm", 2, start), Recorded::New);
        assert_eq!(guilds.at("calm", start), Some(Id::new(2)));
        assert_eq!(
            guilds.at("flood0", start),
            Some(Id::new(1)),
            "nothing is dropped"
        );
    }

    #[test]
    fn a_guild_can_record_again_when_its_tokens_expire() {
        let start = Instant::now();
        let guilds = InteractionGuilds::with_limits(LIMITS);
        for n in 0..LIMITS.per_guild {
            guilds.register_at(&format!("t{n}"), 1, start);
        }
        let expired = start + LIFETIME;

        assert_eq!(guilds.register_at("fresh", 1, expired), Recorded::New);
        assert_eq!(guilds.lock().per_guild.get(&Id::new(1)), Some(&1));
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
        assert_eq!(guilds.lock().per_guild.len(), 1);
    }

    #[test]
    fn only_the_expired_part_of_the_table_is_forgotten() {
        let start = Instant::now();
        let guilds = InteractionGuilds::with_limits(LIMITS);
        guilds.register_at("early", 1, start);
        guilds.register_at("late", 2, start + Duration::from_secs(10));

        let now = start + LIFETIME;
        assert_eq!(guilds.register_at("new", 3, now), Recorded::New);

        assert_eq!(guilds.at("early", now), None);
        assert_eq!(guilds.at("late", now), Some(Id::new(2)));
        assert_eq!(guilds.at("new", now), Some(Id::new(3)));
        assert_eq!(guilds.lock().guilds.len(), 2);
        assert_eq!(guilds.lock().by_expiry.len(), 2);
        assert_eq!(guilds.lock().per_guild.get(&Id::new(1)), None);
    }

    #[test]
    fn recording_a_token_at_the_moment_it_expires_makes_a_new_record() {
        let guilds = InteractionGuilds::with_limits(LIMITS);
        let start = Instant::now();
        guilds.register_at("token", 1, start);

        let at_expiry = start + LIFETIME;
        assert_eq!(guilds.register_at("token", 2, at_expiry), Recorded::New);

        assert_eq!(guilds.at("token", at_expiry), Some(Id::new(2)));
        assert_eq!(guilds.lock().guilds.len(), 1);
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
                guilds.register_at(&format!("extra{n}"), 11, at),
                Recorded::TableFull
            );
        }

        assert_eq!(guilds.lock().examined - examined_before, 1000);
        assert_eq!(guilds.lock().by_expiry.len(), LIMITS.total);
        assert_eq!(guilds.lock().guilds.len(), LIMITS.total);
    }

    #[test]
    fn recording_a_token_again_changes_nothing_and_grows_nothing() {
        let guilds = InteractionGuilds::with_limits(LIMITS);
        let start = Instant::now();
        assert_eq!(guilds.register_at("token", 1, start), Recorded::New);

        // More than the limits, later, and for another guild.
        let later = start + Duration::from_secs(60);
        for _ in 0..=LIMITS.total {
            assert_eq!(guilds.register_at("token", 2, later), Recorded::Known);
        }

        assert_eq!(guilds.at("token", later), Some(Id::new(1)), "first record");
        assert_eq!(guilds.lock().guilds.len(), 1);
        assert_eq!(guilds.lock().by_expiry.len(), 1);
        // And it does not live longer than its first record says.
        assert_eq!(guilds.at("token", start + LIFETIME), None);
    }

    #[test]
    fn the_time_of_a_record_is_read_while_the_lock_is_held() {
        // The first caller is stopped inside the clock. If the clock is read under the lock,
        // the second caller cannot get to its own clock until the first has finished; if it were
        // read before taking the lock, both would already be inside it.
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let calls = Arc::new(AtomicUsize::new(0));
        let guilds = Arc::new(InteractionGuilds::with_clock(LIMITS, {
            let calls = Arc::clone(&calls);
            move || {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    entered_tx.send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                }
                Instant::now()
            }
        }));

        let first = {
            let guilds = Arc::clone(&guilds);
            std::thread::spawn(move || guilds.register(&InteractionToken::new("a"), Id::new(1)))
        };
        entered_rx.recv().unwrap();
        let second = {
            let guilds = Arc::clone(&guilds);
            std::thread::spawn(move || guilds.register(&InteractionToken::new("b"), Id::new(1)))
        };
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the second caller must wait for the lock before it reads the time"
        );

        release_tx.send(()).unwrap();
        assert!(first.join().unwrap());
        assert!(second.join().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn records_made_from_many_threads_stay_in_expiry_order() {
        let guilds = Arc::new(InteractionGuilds::default());
        let threads: Vec<_> = (0..8)
            .map(|thread| {
                let guilds = Arc::clone(&guilds);
                std::thread::spawn(move || {
                    for n in 0..2000 {
                        let token = InteractionToken::new(format!("{thread}-{n}"));
                        assert!(guilds.register(&token, Id::new(1)));
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
