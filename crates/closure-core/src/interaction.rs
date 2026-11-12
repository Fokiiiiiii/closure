//! Which guild each live interaction belongs to. A module is one object shared by every guild, so it
//! can hold on to the client it was given while handling a guild A event and use it with an interaction
//! token it received in guild B. Discord would accept that: the token alone decides where the response
//! goes. The host therefore records, when an interaction arrives, which guild it came from, and
//! [`ApiClient`](crate::client::ApiClient) answers only interactions of the guild it is bound to.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::event::{ChannelId, GuildId, InteractionToken, MessageId, UserId};
use crate::module::ModuleId;
use crate::storage::{MAX_VALUE_BYTES, StorageBackend, StorageError, contains_nul};
use crate::throttle::LogGate;

/// How long Discord honours an interaction token (15 minutes), plus a little slack.
const LIFETIME: Duration = Duration::from_mins(16);

/// How many interactions may be recorded.
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
pub enum Recorded {
    New,
    /// The token was recorded before and still is; the first record is kept.
    Known,
    /// The guild is over its limit.
    GuildFull,
    /// All guilds together are over the limit.
    TableFull,
}

impl Recorded {
    /// Whether the token is recorded now (whether it was new or already known).
    #[must_use]
    pub fn is_recorded(self) -> bool {
        matches!(self, Self::New | Self::Known)
    }
}

#[derive(Default)]
struct Records {
    /// Every token is in here and in `by_expiry` exactly once.
    guilds: HashMap<Arc<str>, (GuildId, Instant)>,
    /// Tokens in the order they were recorded.
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
        // Looked up without creating an entry: a refused guild must leave nothing behind.
        if self.per_guild.get(&guild).copied().unwrap_or(0) >= limits.per_guild {
            return Recorded::GuildFull;
        }
        *self.per_guild.entry(guild).or_insert(0) += 1;
        let token: Arc<str> = Arc::from(token);
        let expires = now + LIFETIME;
        self.guilds.insert(Arc::clone(&token), (guild, expires));
        self.by_expiry.push_back((expires, token));
        Recorded::New
    }

    fn lookup(&self, token: &str, now: Instant) -> Option<GuildId> {
        self.guilds
            .get(token)
            .filter(|(_, expires)| *expires > now)
            .map(|(guild, _)| *guild)
    }
}

type Clock = Box<dyn Fn() -> Instant + Send + Sync>;

/// How far apart the messages about a limit that keeps being reached are.
const LOG_EVERY: Duration = Duration::from_secs(10);

/// The guild of every interaction token that is still valid. Cheap to share behind an `Arc`.
pub struct InteractionGuilds {
    limits: InteractionLimits,
    clock: Clock,
    records: Mutex<Records>,
    guild_full_log: LogGate,
    table_full_log: LogGate,
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
            guild_full_log: LogGate::new(LOG_EVERY),
            table_full_log: LogGate::new(LOG_EVERY),
        }
    }

    /// Records that `token` was issued for an interaction in `guild`. Called by the host for every
    /// interaction it receives, before any module sees it.
    #[must_use]
    pub fn register(&self, token: &InteractionToken, guild: GuildId) -> Recorded {
        // The guard lives for this statement only; the logging below is outside the lock.
        let recorded = self.locked().record(token.expose(), guild);
        // Outside the lock: logging may block. A flood reaches a limit again and again, so the
        // message is let through now and then, with the number of those left out.
        match recorded {
            Recorded::GuildFull => {
                if let Some(held_back) = self.guild_full_log.pass() {
                    tracing::warn!(%guild, held_back, "too many live interactions in this guild; not recording this one");
                }
            }
            Recorded::TableFull => {
                if let Some(held_back) = self.table_full_log.pass() {
                    tracing::warn!(
                        held_back,
                        "too many live interactions; not recording this one"
                    );
                }
            }
            Recorded::New | Recorded::Known => {}
        }
        recorded
    }

    /// The guild `token` was issued for, if it is known and has not expired.
    #[must_use]
    pub fn guild_of(&self, token: &InteractionToken) -> Option<GuildId> {
        self.locked().lookup(token.expose())
    }

    /// The table, locked. This is the only way the clock is read: the time never leaves
    /// [`Locked`], so it is always read, and used, while one and the same guard is held.
    fn locked(&self) -> Locked<'_> {
        Locked {
            records: self.lock(),
            table: self,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Records> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The records with their lock held.
struct Locked<'a> {
    records: MutexGuard<'a, Records>,
    table: &'a InteractionGuilds,
}

impl Locked<'_> {
    fn record(&mut self, token: &str, guild: GuildId) -> Recorded {
        // Read while the lock is held: records are made in the order of their times.
        let now = (self.table.clock)();
        self.records.record(self.table.limits, token, guild, now)
    }

    fn lookup(&self, token: &str) -> Option<GuildId> {
        self.records.lookup(token, (self.table.clock)())
    }
}

/// The message, guild and module that issued a DM component, and its only permitted recipient.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirectMessageComponentRoute {
    pub message_id: MessageId,
    pub channel_id: ChannelId,
    pub guild_id: GuildId,
    pub module_id: ModuleId,
    pub user_id: UserId,
    pub custom_ids: Vec<String>,
}

/// Host-owned, durable provenance for direct-message component clicks.
#[derive(Clone)]
pub struct DirectMessageComponentRoutes {
    backend: Arc<dyn StorageBackend>,
}

impl fmt::Debug for DirectMessageComponentRoutes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DirectMessageComponentRoutes")
            .finish_non_exhaustive()
    }
}

const DM_COMPONENT_ROUTE_TTL: Duration = Duration::from_hours(8760);

impl DirectMessageComponentRoutes {
    #[must_use]
    pub fn new(backend: Arc<dyn StorageBackend>) -> Self {
        Self { backend }
    }

    /// Records a message only once; a retry of the same record is idempotent, a conflicting
    /// message id is refused. Only the host writes this record; modules cannot choose its guild.
    pub async fn record(&self, route: &DirectMessageComponentRoute) -> Result<(), StorageError> {
        if route.message_id.get() == 0 {
            return Err(StorageError::Value("message id must be positive".into()));
        }
        let value =
            serde_json::to_value(route).map_err(|error| StorageError::Value(error.to_string()))?;
        let size = serde_json::to_vec(&value).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_VALUE_BYTES {
            return Err(StorageError::TooLarge {
                size,
                limit: MAX_VALUE_BYTES,
            });
        }
        if contains_nul(&value) {
            return Err(StorageError::Value("text must not contain NUL".into()));
        }
        match self
            .backend
            .insert_direct_message_component_route(route.message_id, value, DM_COMPONENT_ROUTE_TTL)
            .await
        {
            Ok(()) => Ok(()),
            Err(StorageError::Conflict) => {
                if self.get(route.message_id).await?.as_ref() == Some(route) {
                    Ok(())
                } else {
                    Err(StorageError::Conflict)
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Finds the record by Discord message id; callers still compare channel, user and custom id.
    pub async fn get(
        &self,
        message_id: MessageId,
    ) -> Result<Option<DirectMessageComponentRoute>, StorageError> {
        self.backend
            .get_direct_message_component_route(message_id)
            .await?
            .map(|entry| {
                serde_json::from_value(entry.value)
                    .map_err(|error| StorageError::Value(error.to_string()))
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {

    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::mpsc;

    use twilight_model::id::Id;

    use super::*;

    const LIMITS: InteractionLimits = InteractionLimits {
        total: 100,
        per_guild: 40,
    };

    impl InteractionGuilds {
        fn register_at(&self, token: &str, guild: u64, now: Instant) -> Recorded {
            self.lock().record(self.limits, token, Id::new(guild), now)
        }

        fn at(&self, token: &str, now: Instant) -> Option<GuildId> {
            self.lock().lookup(token, now)
        }
    }

    #[tokio::test]
    async fn dm_component_routes_are_persistent_and_bound_to_one_message_and_recipient() {
        let routes =
            DirectMessageComponentRoutes::new(Arc::new(crate::storage::MemoryStorage::default()));
        let route = DirectMessageComponentRoute {
            message_id: Id::new(10),
            channel_id: Id::new(11),
            guild_id: Id::new(12),
            module_id: ModuleId::new("tickets").unwrap(),
            user_id: Id::new(13),
            custom_ids: vec!["tickets:rate:13:12:44:1:5".into()],
        };

        routes.record(&route).await.unwrap();
        assert_eq!(
            routes.get(route.message_id).await.unwrap(),
            Some(route.clone())
        );
        assert_eq!(routes.get(Id::new(14)).await.unwrap(), None);
        routes.record(&route).await.unwrap();

        let mut forged = route.clone();
        forged.user_id = Id::new(15);
        assert_eq!(routes.record(&forged).await, Err(StorageError::Conflict));
    }

    #[tokio::test]
    async fn dm_component_routes_expire_after_one_year() {
        let now = Arc::new(AtomicI64::new(1_700_000_000));
        let clock = Arc::clone(&now);
        let routes = DirectMessageComponentRoutes::new(Arc::new(
            crate::storage::MemoryStorage::with_clock(move || clock.load(Ordering::SeqCst)),
        ));
        let route = DirectMessageComponentRoute {
            message_id: Id::new(20),
            channel_id: Id::new(21),
            guild_id: Id::new(22),
            module_id: ModuleId::new("tickets").unwrap(),
            user_id: Id::new(23),
            custom_ids: vec!["tickets:rate:23:22:44:1:5".into()],
        };
        routes.record(&route).await.unwrap();
        assert!(routes.get(route.message_id).await.unwrap().is_some());

        now.fetch_add(365 * 24 * 60 * 60, Ordering::SeqCst);
        assert_eq!(routes.get(route.message_id).await.unwrap(), None);
    }

    #[test]
    fn a_token_belongs_to_the_guild_it_was_registered_for() {
        let guilds = InteractionGuilds::default();
        let (a, b) = (
            InteractionToken::new("token-a"),
            InteractionToken::new("token-b"),
        );
        assert!(guilds.register(&a, Id::new(1)).is_recorded());
        assert!(guilds.register(&b, Id::new(2)).is_recorded());

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
        assert!(
            guilds
                .register(&InteractionToken::new("secret-token"), Id::new(1))
                .is_recorded()
        );
        assert!(!format!("{guilds:?}").contains("secret-token"));
    }

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

        let later = start + Duration::from_secs(60);
        for _ in 0..=LIMITS.total {
            assert_eq!(guilds.register_at("token", 2, later), Recorded::Known);
        }

        assert_eq!(guilds.at("token", later), Some(Id::new(1)), "first record");
        assert_eq!(guilds.lock().guilds.len(), 1);
        assert_eq!(guilds.lock().by_expiry.len(), 1);
        assert_eq!(guilds.at("token", start + LIFETIME), None);
    }

    #[test]
    fn the_time_of_a_record_is_read_while_the_lock_is_held() {
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
        let (started_tx, started_rx) = mpsc::channel();
        let second = {
            let guilds = Arc::clone(&guilds);
            std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                guilds.register(&InteractionToken::new("b"), Id::new(1))
            })
        };
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the second caller must wait for the lock before it reads the time"
        );

        release_tx.send(()).unwrap();
        assert!(first.join().unwrap().is_recorded());
        assert!(second.join().unwrap().is_recorded());
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
                        assert!(guilds.register(&token, Id::new(1)).is_recorded());
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

    #[test]
    fn a_guild_with_no_room_leaves_nothing_behind() {
        let guilds = InteractionGuilds::with_limits(InteractionLimits {
            total: 100,
            per_guild: 0,
        });
        let now = Instant::now();
        for guild in 1..=50 {
            assert_eq!(
                guilds.register_at(&format!("t{guild}"), guild, now),
                Recorded::GuildFull
            );
        }
        assert!(guilds.lock().per_guild.is_empty());
        assert!(guilds.lock().guilds.is_empty());
    }

    #[test]
    fn the_caller_is_told_which_limit_was_reached() {
        let guilds = InteractionGuilds::with_limits(InteractionLimits {
            total: 3,
            per_guild: 2,
        });
        let register = |token: &str, guild: u64| {
            guilds.register(&InteractionToken::new(token), Id::new(guild))
        };

        assert_eq!(register("a", 1), Recorded::New);
        assert_eq!(register("a", 1), Recorded::Known);
        assert_eq!(register("b", 1), Recorded::New);
        assert_eq!(register("c", 1), Recorded::GuildFull);
        assert_eq!(register("d", 2), Recorded::New);
        assert_eq!(register("e", 3), Recorded::TableFull);
        assert!(!Recorded::GuildFull.is_recorded() && !Recorded::TableFull.is_recorded());
    }
}
