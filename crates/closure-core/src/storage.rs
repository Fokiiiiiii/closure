//! Data a module keeps: entries (a key and a JSON value) and counters.
//!
//! A module never sees a database. It holds a [`Store`], which is bound to the module and the
//! guild of the event, so it cannot read or write anything else's data; the host decides where
//! the data lives ([`StorageBackend`]: PostgreSQL in the bot, [`MemoryStorage`] in tests).
//! Everything is limited: keys, values, how many entries and counters a module may keep in a
//! guild; going over a limit is an error, never silent loss.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::capability::{Capability, CapabilitySet};
use crate::event::GuildId;
use crate::module::ModuleId;

/// The longest key, in characters.
pub const MAX_KEY_CHARS: usize = 128;
/// The largest value, as JSON text.
pub const MAX_VALUE_BYTES: usize = 32 * 1024;
/// How many entries a module may keep in one guild.
pub const MAX_ENTRIES: usize = 10_000;
/// How many counters (a name and a subject) a module may keep in one guild.
pub const MAX_COUNTERS: usize = 100_000;
/// The widest window of [`Store::counter_recent`], and how long the minutes behind it are kept.
pub const MAX_WINDOW: Duration = Duration::from_secs(90 * 24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageError {
    #[error("the module is not granted the capability `storage`")]
    NotGranted,
    #[error("invalid key: {0}")]
    InvalidKey(String),
    #[error("invalid counter: {0}")]
    InvalidCounter(String),
    #[error("the value is {size} bytes; at most {limit} are allowed")]
    TooLarge { size: usize, limit: usize },
    #[error("the module already keeps {limit} of these in this guild")]
    QuotaExceeded { limit: usize },
    /// The entry is not as the caller expected: it changed since it was read.
    #[error("the entry changed since it was read")]
    Conflict,
    #[error("no storage is available here")]
    Unavailable,
    #[error("the value cannot be stored: {0}")]
    Value(String),
    #[error("storage failed: {0}")]
    Backend(String),
}

/// A stored value and the revision it was written at.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub value: serde_json::Value,
    /// A number that changes with every write. No two writes share one, not even those of an
    /// entry that was deleted (or expired) and made again, so a revision read earlier can
    /// never match a later entry by accident.
    pub revision: i64,
}

/// What a write expects to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    Any,
    /// No entry under the key (or only an expired one).
    Missing,
    /// The entry is at this revision.
    Revision(i64),
}

/// Where the data lives.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    async fn get(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<Option<Entry>, StorageError>;

    /// Writes the entry and returns its new revision, or fails with [`StorageError::Conflict`]
    /// if it is not as `expect` says.
    async fn put(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
        value: serde_json::Value,
        ttl: Option<Duration>,
        expect: Expect,
    ) -> Result<i64, StorageError>;

    /// Whether there was an entry to remove.
    async fn delete(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<bool, StorageError>;

    /// Entries whose key starts with `prefix`, in key order.
    async fn list(
        &self,
        module: &ModuleId,
        guild: GuildId,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Entry)>, StorageError>;

    /// Adds `delta` to a counter, in its total and in the current minute; returns the total.
    async fn counter_add(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
        delta: i64,
    ) -> Result<i64, StorageError>;

    /// The total, or the sum over the last `window` minutes (rounded up; the current minute
    /// counts as the first).
    async fn counter_get(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
        window: Option<Duration>,
    ) -> Result<i64, StorageError>;

    async fn counter_reset(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
    ) -> Result<(), StorageError>;
}

/// Whether `key` is one the store accepts: 1 to 128 printable ASCII characters.
pub fn check_key(key: &str) -> Result<(), StorageError> {
    let ok =
        (1..=MAX_KEY_CHARS).contains(&key.len()) && key.bytes().all(|b| (0x21..=0x7e).contains(&b));
    if ok {
        Ok(())
    } else {
        Err(StorageError::InvalidKey(format!(
            "expected 1-{MAX_KEY_CHARS} printable ASCII characters without spaces"
        )))
    }
}

fn check_counter(name: &str) -> Result<(), StorageError> {
    check_key(name).map_err(|_| {
        StorageError::InvalidCounter(format!(
            "the name must be 1-{MAX_KEY_CHARS} printable ASCII characters without spaces"
        ))
    })
}

/// PostgreSQL cannot keep U+0000 in JSON text, so no backend takes it.
fn contains_nul(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains('\0'),
        serde_json::Value::Array(items) => items.iter().any(contains_nul),
        serde_json::Value::Object(map) => map
            .iter()
            .any(|(key, item)| key.contains('\0') || contains_nul(item)),
        _ => false,
    }
}

/// The minutes a window covers, at least one.
#[must_use]
pub fn window_minutes(window: Duration) -> i64 {
    i64::try_from(window.min(MAX_WINDOW).as_secs().div_ceil(60).max(1)).unwrap_or(i64::MAX)
}

/// A module's data in one guild.
#[derive(Clone)]
pub struct Store {
    backend: Arc<dyn StorageBackend>,
    granted: bool,
    module: ModuleId,
    guild: GuildId,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("module", &self.module)
            .field("guild", &self.guild)
            .finish_non_exhaustive()
    }
}

/// The backend of a [`Store`] that has none.
struct NoStorage;

#[async_trait]
impl StorageBackend for NoStorage {
    async fn get(&self, _: &ModuleId, _: GuildId, _: &str) -> Result<Option<Entry>, StorageError> {
        Err(StorageError::Unavailable)
    }

    async fn put(
        &self,
        _: &ModuleId,
        _: GuildId,
        _: &str,
        _: serde_json::Value,
        _: Option<Duration>,
        _: Expect,
    ) -> Result<i64, StorageError> {
        Err(StorageError::Unavailable)
    }

    async fn delete(&self, _: &ModuleId, _: GuildId, _: &str) -> Result<bool, StorageError> {
        Err(StorageError::Unavailable)
    }

    async fn list(
        &self,
        _: &ModuleId,
        _: GuildId,
        _: &str,
        _: usize,
    ) -> Result<Vec<(String, Entry)>, StorageError> {
        Err(StorageError::Unavailable)
    }

    async fn counter_add(
        &self,
        _: &ModuleId,
        _: GuildId,
        _: &str,
        _: u64,
        _: i64,
    ) -> Result<i64, StorageError> {
        Err(StorageError::Unavailable)
    }

    async fn counter_get(
        &self,
        _: &ModuleId,
        _: GuildId,
        _: &str,
        _: u64,
        _: Option<Duration>,
    ) -> Result<i64, StorageError> {
        Err(StorageError::Unavailable)
    }

    async fn counter_reset(
        &self,
        _: &ModuleId,
        _: GuildId,
        _: &str,
        _: u64,
    ) -> Result<(), StorageError> {
        Err(StorageError::Unavailable)
    }
}

impl Store {
    /// The store of `module` in `guild`; usable only if `granted` holds [`Capability::Storage`].
    #[must_use]
    pub fn new(
        backend: Arc<dyn StorageBackend>,
        granted: &CapabilitySet,
        module: ModuleId,
        guild: GuildId,
    ) -> Self {
        Self {
            backend,
            granted: granted.contains(Capability::Storage),
            module,
            guild,
        }
    }

    /// A store that fails every call, for contexts that have no storage.
    #[must_use]
    pub fn unavailable(module: ModuleId, guild: GuildId) -> Self {
        Self {
            backend: Arc::new(NoStorage),
            granted: true,
            module,
            guild,
        }
    }

    fn allowed(&self) -> Result<(), StorageError> {
        if self.granted {
            Ok(())
        } else {
            Err(StorageError::NotGranted)
        }
    }

    /// The entry under `key`, with its revision.
    pub async fn entry(&self, key: &str) -> Result<Option<Entry>, StorageError> {
        self.allowed()?;
        check_key(key)?;
        self.backend.get(&self.module, self.guild, key).await
    }

    /// The value under `key`, as `T`.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StorageError> {
        self.entry(key)
            .await?
            .map(|entry| {
                serde_json::from_value(entry.value)
                    .map_err(|error| StorageError::Value(error.to_string()))
            })
            .transpose()
    }

    /// Writes the value under `key`, whatever was there.
    pub async fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<(), StorageError> {
        self.write(key, value, None, Expect::Any).await.map(drop)
    }

    /// Writes the value, which is gone after `ttl`.
    pub async fn set_for<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Duration,
    ) -> Result<(), StorageError> {
        self.write(key, value, Some(ttl), Expect::Any)
            .await
            .map(drop)
    }

    /// Writes the value only if the entry is as `expect` says (see [`Entry::revision`]); that
    /// is how two handlers that read, change and write the same entry avoid losing an update.
    /// Returns the new revision.
    pub async fn replace<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        expect: Expect,
        ttl: Option<Duration>,
    ) -> Result<i64, StorageError> {
        self.write(key, value, ttl, expect).await
    }

    async fn write<T: Serialize>(
        &self,
        key: &str,
        value: &T,
        ttl: Option<Duration>,
        expect: Expect,
    ) -> Result<i64, StorageError> {
        self.allowed()?;
        check_key(key)?;
        let value =
            serde_json::to_value(value).map_err(|error| StorageError::Value(error.to_string()))?;
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
        self.backend
            .put(&self.module, self.guild, key, value, ttl, expect)
            .await
    }

    /// Whether there was an entry to delete.
    pub async fn delete(&self, key: &str) -> Result<bool, StorageError> {
        self.allowed()?;
        check_key(key)?;
        self.backend.delete(&self.module, self.guild, key).await
    }

    /// Up to `limit` entries whose key starts with `prefix`, in key order.
    pub async fn list(
        &self,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Entry)>, StorageError> {
        self.allowed()?;
        if !prefix.is_empty() {
            check_key(prefix)?;
        }
        self.backend
            .list(&self.module, self.guild, prefix, limit.min(1000))
            .await
    }

    /// Adds `delta` (it may be negative) to the counter `name` of `subject` (a user, a role,
    /// anything with a number) and returns the new total.
    pub async fn counter_add(
        &self,
        name: &str,
        subject: impl Into<u64>,
        delta: i64,
    ) -> Result<i64, StorageError> {
        self.allowed()?;
        check_counter(name)?;
        self.backend
            .counter_add(&self.module, self.guild, name, subject.into(), delta)
            .await
    }

    /// The total of the counter.
    pub async fn counter_total(
        &self,
        name: &str,
        subject: impl Into<u64>,
    ) -> Result<i64, StorageError> {
        self.allowed()?;
        check_counter(name)?;
        self.backend
            .counter_get(&self.module, self.guild, name, subject.into(), None)
            .await
    }

    /// What was added in the last `window` (at most 90 days), counted in whole minutes rounded
    /// up, the current one included: five minutes is this minute and the four before it.
    pub async fn counter_recent(
        &self,
        name: &str,
        subject: impl Into<u64>,
        window: Duration,
    ) -> Result<i64, StorageError> {
        self.allowed()?;
        check_counter(name)?;
        self.backend
            .counter_get(&self.module, self.guild, name, subject.into(), Some(window))
            .await
    }

    /// Sets the counter back to nothing, history included.
    pub async fn counter_reset(
        &self,
        name: &str,
        subject: impl Into<u64>,
    ) -> Result<(), StorageError> {
        self.allowed()?;
        check_counter(name)?;
        self.backend
            .counter_reset(&self.module, self.guild, name, subject.into())
            .await
    }
}

type RowKey = (ModuleId, GuildId, String);
type CounterKey = (ModuleId, GuildId, String, u64);

#[derive(Debug, Clone)]
struct Row {
    entry: Entry,
    /// Unix seconds.
    expires: Option<i64>,
}

#[derive(Debug, Default, Clone)]
struct CounterRow {
    total: i64,
    /// Minutes since the epoch to what was added in them.
    minutes: BTreeMap<i64, i64>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The revision of the latest write; every write takes the next one.
    revisions: i64,
    entries: HashMap<RowKey, Row>,
    counters: HashMap<CounterKey, CounterRow>,
}

type UnixClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// [`StorageBackend`] in memory, with the same limits as the real one. For tests and for
/// running without a database.
pub struct MemoryStorage {
    clock: UnixClock,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for MemoryStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStorage").finish_non_exhaustive()
    }
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::with_clock(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
                })
        })
    }
}

impl MemoryStorage {
    /// Storage whose idea of the time (unix seconds) is `clock`, so tests can move it.
    #[must_use]
    pub fn with_clock(clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        Self {
            clock: Arc::new(clock),
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn live(row: &Row, now: i64) -> bool {
        row.expires.is_none_or(|end| end > now)
    }
}

#[async_trait]
impl StorageBackend for MemoryStorage {
    async fn get(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<Option<Entry>, StorageError> {
        let now = (self.clock)();
        Ok(self
            .lock()
            .entries
            .get(&(module.clone(), guild, key.to_owned()))
            .filter(|row| Self::live(row, now))
            .map(|row| row.entry.clone()))
    }

    async fn put(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
        value: serde_json::Value,
        ttl: Option<Duration>,
        expect: Expect,
    ) -> Result<i64, StorageError> {
        let now = (self.clock)();
        let mut inner = self.lock();
        let row_key = (module.clone(), guild, key.to_owned());
        let current = inner
            .entries
            .get(&row_key)
            .filter(|row| Self::live(row, now));
        let revision = current.map(|row| row.entry.revision);
        match expect {
            Expect::Any => {}
            Expect::Missing if revision.is_none() => {}
            Expect::Revision(wanted) if revision == Some(wanted) => {}
            _ => return Err(StorageError::Conflict),
        }
        if revision.is_none() {
            let kept = inner
                .entries
                .iter()
                .filter(|((m, g, _), row)| m == module && *g == guild && Self::live(row, now))
                .count();
            if kept >= MAX_ENTRIES {
                return Err(StorageError::QuotaExceeded { limit: MAX_ENTRIES });
            }
        }
        inner.revisions += 1;
        let revision = inner.revisions;
        let expires = ttl
            .map(|ttl| now.saturating_add(i64::try_from(ttl.as_secs().max(1)).unwrap_or(i64::MAX)));
        inner.entries.insert(
            row_key,
            Row {
                entry: Entry { value, revision },
                expires,
            },
        );
        Ok(revision)
    }

    async fn delete(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<bool, StorageError> {
        let now = (self.clock)();
        Ok(self
            .lock()
            .entries
            .remove(&(module.clone(), guild, key.to_owned()))
            .is_some_and(|row| Self::live(&row, now)))
    }

    async fn list(
        &self,
        module: &ModuleId,
        guild: GuildId,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<(String, Entry)>, StorageError> {
        let now = (self.clock)();
        let inner = self.lock();
        let mut found: Vec<(String, Entry)> = inner
            .entries
            .iter()
            .filter(|((m, g, key), row)| {
                m == module && *g == guild && key.starts_with(prefix) && Self::live(row, now)
            })
            .map(|((_, _, key), row)| (key.clone(), row.entry.clone()))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found.truncate(limit);
        Ok(found)
    }

    async fn counter_add(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
        delta: i64,
    ) -> Result<i64, StorageError> {
        let minute = (self.clock)().div_euclid(60);
        let mut inner = self.lock();
        let key = (module.clone(), guild, counter.to_owned(), subject);
        if !inner.counters.contains_key(&key) {
            let kept = inner
                .counters
                .keys()
                .filter(|(m, g, _, _)| m == module && *g == guild)
                .count();
            if kept >= MAX_COUNTERS {
                return Err(StorageError::QuotaExceeded {
                    limit: MAX_COUNTERS,
                });
            }
        }
        let row = inner.counters.entry(key).or_default();
        row.total = row
            .total
            .checked_add(delta)
            .ok_or_else(|| StorageError::Backend("the counter overflows".into()))?;
        *row.minutes.entry(minute).or_default() += delta;
        let oldest = minute - window_minutes(MAX_WINDOW);
        row.minutes.retain(|at, _| *at > oldest);
        Ok(row.total)
    }

    async fn counter_get(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
        window: Option<Duration>,
    ) -> Result<i64, StorageError> {
        let minute = (self.clock)().div_euclid(60);
        let inner = self.lock();
        let Some(row) = inner
            .counters
            .get(&(module.clone(), guild, counter.to_owned(), subject))
        else {
            return Ok(0);
        };
        Ok(match window {
            None => row.total,
            Some(window) => {
                let from = minute - window_minutes(window) + 1;
                row.minutes.range(from..).map(|(_, value)| value).sum()
            }
        })
    }

    async fn counter_reset(
        &self,
        module: &ModuleId,
        guild: GuildId,
        counter: &str,
        subject: u64,
    ) -> Result<(), StorageError> {
        self.lock()
            .counters
            .remove(&(module.clone(), guild, counter.to_owned(), subject));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use serde_json::json;
    use twilight_model::id::Id;

    use super::*;

    fn module(name: &str) -> ModuleId {
        ModuleId::new(name).unwrap()
    }

    fn granted() -> CapabilitySet {
        [Capability::Storage].into_iter().collect()
    }

    struct Fixture {
        backend: Arc<MemoryStorage>,
        now: Arc<AtomicI64>,
    }

    fn fixture() -> Fixture {
        let now = Arc::new(AtomicI64::new(1_700_000_000));
        let clock = Arc::clone(&now);
        Fixture {
            backend: Arc::new(MemoryStorage::with_clock(move || {
                clock.load(Ordering::SeqCst)
            })),
            now,
        }
    }

    impl Fixture {
        fn store(&self, module_name: &str, guild: u64) -> Store {
            Store::new(
                Arc::clone(&self.backend) as Arc<dyn StorageBackend>,
                &granted(),
                module(module_name),
                Id::new(guild),
            )
        }

        fn advance(&self, seconds: i64) {
            self.now.fetch_add(seconds, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn values_are_kept_per_module_and_guild() {
        let f = fixture();
        let (warn, other, elsewhere) = (f.store("warn", 1), f.store("mute", 1), f.store("warn", 2));
        warn.set("config", &json!({ "a": 1 })).await.unwrap();
        assert_eq!(
            warn.get::<serde_json::Value>("config").await.unwrap(),
            Some(json!({ "a": 1 }))
        );
        assert_eq!(
            other.get::<serde_json::Value>("config").await.unwrap(),
            None
        );
        assert_eq!(
            elsewhere.get::<serde_json::Value>("config").await.unwrap(),
            None
        );
        assert!(warn.delete("config").await.unwrap());
        assert!(!warn.delete("config").await.unwrap());
        assert_eq!(warn.get::<serde_json::Value>("config").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_value_with_a_time_to_live_is_gone_afterwards() {
        let f = fixture();
        let store = f.store("warn", 1);
        store
            .set_for("session", &"x", Duration::from_secs(60))
            .await
            .unwrap();
        f.advance(59);
        assert_eq!(
            store.get::<String>("session").await.unwrap().as_deref(),
            Some("x")
        );
        f.advance(1);
        assert_eq!(store.get::<String>("session").await.unwrap(), None);
        // An expired entry counts as missing for a write that expects none.
        assert_eq!(
            store
                .replace("session", &"y", Expect::Missing, None)
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn two_writers_cannot_both_win_with_the_same_revision() {
        let f = fixture();
        let store = f.store("warn", 1);
        assert_eq!(
            store.replace("n", &1, Expect::Missing, None).await.unwrap(),
            1
        );
        assert_eq!(
            store.replace("n", &9, Expect::Missing, None).await,
            Err(StorageError::Conflict)
        );
        let revision = store.entry("n").await.unwrap().unwrap().revision;
        assert_eq!(
            store
                .replace("n", &2, Expect::Revision(revision), None)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .replace("n", &3, Expect::Revision(revision), None)
                .await,
            Err(StorageError::Conflict)
        );
        assert_eq!(store.get::<i64>("n").await.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn a_revision_read_before_a_delete_never_matches_the_entry_made_again() {
        let f = fixture();
        let store = f.store("warn", 1);
        store.set("n", &"first").await.unwrap();
        let old = store.entry("n").await.unwrap().unwrap().revision;
        store.delete("n").await.unwrap();
        store.set("n", &"second").await.unwrap();
        assert_eq!(
            store
                .replace("n", &"stale", Expect::Revision(old), None)
                .await,
            Err(StorageError::Conflict)
        );
        assert_eq!(
            store.get::<String>("n").await.unwrap().as_deref(),
            Some("second")
        );

        // The same when the first one ran out instead of being deleted.
        store
            .set_for("t", &"first", Duration::from_secs(1))
            .await
            .unwrap();
        let old = store.entry("t").await.unwrap().unwrap().revision;
        f.advance(2);
        store.set("t", &"second").await.unwrap();
        assert_eq!(
            store
                .replace("t", &"stale", Expect::Revision(old), None)
                .await,
            Err(StorageError::Conflict)
        );
    }

    #[tokio::test]
    async fn entries_are_listed_by_prefix_in_key_order() {
        let f = fixture();
        let store = f.store("warn", 1);
        for key in ["case:3", "case:1", "note:1", "case:2"] {
            store.set(key, &key).await.unwrap();
        }
        let keys: Vec<String> = store
            .list("case:", 2)
            .await
            .unwrap()
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, ["case:1", "case:2"]);
        assert_eq!(store.list("", 10).await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn keys_values_and_the_capability_are_checked() {
        let f = fixture();
        let store = f.store("warn", 1);
        for key in ["", "has space", "ünï", &"k".repeat(129)] {
            assert!(
                matches!(store.set(key, &1).await, Err(StorageError::InvalidKey(_))),
                "{key:?}"
            );
        }
        let big = "x".repeat(MAX_VALUE_BYTES);
        assert!(matches!(
            store.set("big", &big).await,
            Err(StorageError::TooLarge { .. })
        ));
        assert!(matches!(
            store.set("nul", &"a\0b").await,
            Err(StorageError::Value(_))
        ));
        assert!(matches!(
            store.set("nul", &json!({ "k": ["a\0"] })).await,
            Err(StorageError::Value(_))
        ));
        // Text that merely looks like an escaped NUL is fine.
        store.set("looks-like", &"\\u0000").await.unwrap();
        let none = Store::new(
            Arc::clone(&f.backend) as Arc<dyn StorageBackend>,
            &CapabilitySet::new(),
            module("warn"),
            Id::new(1),
        );
        assert_eq!(none.set("k", &1).await, Err(StorageError::NotGranted));
        assert_eq!(
            none.counter_add("c", 1_u64, 1).await,
            Err(StorageError::NotGranted)
        );
        let unavailable = Store::unavailable(module("warn"), Id::new(1));
        assert_eq!(
            unavailable.get::<i64>("k").await,
            Err(StorageError::Unavailable)
        );
    }

    #[tokio::test]
    async fn a_module_cannot_keep_more_entries_than_the_quota_but_may_replace_them() {
        let f = fixture();
        let store = f.store("warn", 1);
        for n in 0..MAX_ENTRIES {
            store.set(&format!("k{n}"), &n).await.unwrap();
        }
        assert_eq!(
            store.set("one-too-many", &1).await,
            Err(StorageError::QuotaExceeded { limit: MAX_ENTRIES })
        );
        store.set("k0", &"changed").await.unwrap();
        // Another guild has its own room, and so does a deleted key.
        f.store("warn", 2).set("one-too-many", &1).await.unwrap();
        assert!(store.delete("k1").await.unwrap());
        store.set("one-too-many", &1).await.unwrap();
    }

    #[tokio::test]
    async fn counters_add_up_per_subject_and_over_a_window() {
        let f = fixture();
        let store = f.store("warn", 1);
        assert_eq!(store.counter_total("warns", 7_u64).await.unwrap(), 0);
        assert_eq!(store.counter_add("warns", 7_u64, 1).await.unwrap(), 1);
        f.advance(120);
        assert_eq!(store.counter_add("warns", 7_u64, 2).await.unwrap(), 3);
        assert_eq!(store.counter_add("warns", 8_u64, 5).await.unwrap(), 5);
        assert_eq!(store.counter_total("warns", 7_u64).await.unwrap(), 3);
        // This minute and the one before it: the first warning is two minutes old.
        assert_eq!(
            store
                .counter_recent("warns", 7_u64, Duration::from_secs(120))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .counter_recent("warns", 7_u64, Duration::from_secs(180))
                .await
                .unwrap(),
            3
        );
        // Less than a minute still means the current minute.
        assert_eq!(
            store
                .counter_recent("warns", 7_u64, Duration::from_secs(1))
                .await
                .unwrap(),
            2
        );
        store.counter_reset("warns", 7_u64).await.unwrap();
        assert_eq!(store.counter_total("warns", 7_u64).await.unwrap(), 0);
        assert_eq!(
            store
                .counter_recent("warns", 7_u64, Duration::from_secs(600))
                .await
                .unwrap(),
            0
        );
        assert_eq!(store.counter_total("warns", 8_u64).await.unwrap(), 5);
        // Counters are the module's own, in its own guild.
        assert_eq!(
            f.store("mute", 1)
                .counter_total("warns", 8_u64)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            f.store("warn", 2)
                .counter_total("warns", 8_u64)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_counter_that_would_overflow_is_an_error_and_names_are_checked() {
        let f = fixture();
        let store = f.store("warn", 1);
        store.counter_add("big", 1_u64, i64::MAX).await.unwrap();
        assert!(matches!(
            store.counter_add("big", 1_u64, 1).await,
            Err(StorageError::Backend(_))
        ));
        assert_eq!(store.counter_total("big", 1_u64).await.unwrap(), i64::MAX);
        assert!(matches!(
            store.counter_add("bad name", 1_u64, 1).await,
            Err(StorageError::InvalidCounter(_))
        ));
    }
}
