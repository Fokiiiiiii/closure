//! Work a module asks to have done later: once, or again and again.
//!
//! A module holds a [`Scheduler`] bound to itself and the guild of the event. A job is a name
//! and a JSON payload; when it is due the host delivers it to the module as an
//! [`Event::Scheduled`](crate::event::Event::Scheduled). Delivery is *at least once*: if the bot
//! stops while a job runs, or the handler fails, the job comes again (a failed one after a
//! growing pause, up to [`MAX_ATTEMPTS`] times), so a handler has to be safe to run twice.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Serialize;

use crate::capability::{Capability, CapabilitySet};
use crate::event::GuildId;
use crate::module::ModuleId;
use crate::storage::check_key;

/// How many times a job is tried before it is given up on (and kept for a look).
pub const MAX_ATTEMPTS: u32 = 8;
/// How many jobs a module may have waiting in one guild.
pub const MAX_JOBS: usize = 10_000;
/// The largest payload, as JSON text.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
/// The shortest time between two runs of a repeating job.
pub const MIN_EVERY: Duration = Duration::from_secs(10);
/// How far ahead a job may be set (and the longest time between two runs).
pub const MAX_DELAY: Duration = Duration::from_secs(10 * 365 * 24 * 3600);
/// The last time a job may be set for, in unix seconds: the end of the year 9999.
pub const MAX_AT: i64 = 253_402_300_799;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScheduleError {
    #[error("the module is not granted the capability `schedule`")]
    NotGranted,
    #[error("invalid name or key: {0}")]
    InvalidName(String),
    #[error("a job repeats at most every {} seconds", MIN_EVERY.as_secs())]
    TooOften,
    #[error("a job is set at most 10 years ahead, and for a time between 1970 and 9999")]
    OutOfRange,
    #[error("the payload is {size} bytes; at most {limit} are allowed")]
    TooLarge { size: usize, limit: usize },
    #[error("the module already has {limit} jobs waiting in this guild")]
    QuotaExceeded { limit: usize },
    #[error("the payload cannot be stored: {0}")]
    Payload(String),
    #[error("no scheduler is available here")]
    Unavailable,
    #[error("scheduling failed: {0}")]
    Backend(String),
}

/// When a job is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunAt {
    /// This long from now, by the clock of the scheduler.
    After(Duration),
    /// At this time, in unix seconds.
    At(i64),
}

/// A job to schedule.
#[derive(Debug, Clone, PartialEq)]
pub struct NewJob {
    pub module: ModuleId,
    pub guild: GuildId,
    pub name: String,
    pub payload: serde_json::Value,
    pub run_at: RunAt,
    /// Run again this long after the planned time of each run.
    pub every: Option<Duration>,
    /// A job with the same key (of the same module and guild) is replaced instead of added.
    pub key: Option<String>,
}

/// A job that is due, as the module gets it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScheduledJob {
    pub id: i64,
    pub guild_id: GuildId,
    pub module: ModuleId,
    pub name: String,
    pub payload: serde_json::Value,
    /// Which try this is: 1 the first time.
    pub attempt: u32,
    /// Tells this hand-out from every other one. What the host reports back about the job only
    /// counts while the job is still held under this token: if the job was replaced (same key)
    /// or handed out again meanwhile, the report is about something that is gone.
    pub token: i64,
}

/// Where jobs are kept and handed out.
#[async_trait]
pub trait SchedulerBackend: Send + Sync {
    async fn schedule(&self, job: NewJob) -> Result<i64, ScheduleError>;

    /// Removes the job with `key`; whether there was one.
    async fn cancel(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<bool, ScheduleError>;

    /// Takes up to `limit` jobs that are due and not being run, and holds them for `lease`; a job
    /// not reported done by then is handed out again. Each comes with its attempt counted.
    async fn claim_due(
        &self,
        limit: usize,
        lease: Duration,
    ) -> Result<Vec<ScheduledJob>, ScheduleError>;

    /// The job ran: a repeating one is set for its next time, any other is removed. Does nothing
    /// unless the job is still held under `token` (see [`ScheduledJob::token`]); so do the
    /// other calls that report on a handed-out job.
    async fn complete(&self, id: i64, token: i64) -> Result<(), ScheduleError>;

    /// The job could not be run for a reason that is not its fault (its module is switched off,
    /// the bot is busy): hand it out again after `after`, without counting the try.
    async fn postpone(&self, id: i64, token: i64, after: Duration) -> Result<(), ScheduleError>;

    /// The job failed: try again after `after`, or, if it has been tried [`MAX_ATTEMPTS`] times,
    /// keep it as failed and no longer hand it out.
    async fn retry(
        &self,
        id: i64,
        token: i64,
        after: Duration,
        error: &str,
    ) -> Result<(), ScheduleError>;

    /// Removes a job that can never run (its module is gone).
    async fn discard(&self, id: i64, token: i64) -> Result<(), ScheduleError>;
}

/// The wait before the next try of a job that has been tried `attempt` times.
#[must_use]
pub fn backoff(attempt: u32) -> Duration {
    let seconds = 5_u64.saturating_mul(1 << attempt.saturating_sub(1).min(20));
    Duration::from_secs(seconds.min(3600))
}

/// A module's way to have work done later, in one guild.
#[derive(Clone)]
pub struct Scheduler {
    backend: Arc<dyn SchedulerBackend>,
    granted: bool,
    module: ModuleId,
    guild: GuildId,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("module", &self.module)
            .field("guild", &self.guild)
            .finish_non_exhaustive()
    }
}

struct NoScheduler;

#[async_trait]
impl SchedulerBackend for NoScheduler {
    async fn schedule(&self, _: NewJob) -> Result<i64, ScheduleError> {
        Err(ScheduleError::Unavailable)
    }

    async fn cancel(&self, _: &ModuleId, _: GuildId, _: &str) -> Result<bool, ScheduleError> {
        Err(ScheduleError::Unavailable)
    }

    async fn claim_due(&self, _: usize, _: Duration) -> Result<Vec<ScheduledJob>, ScheduleError> {
        Err(ScheduleError::Unavailable)
    }

    async fn complete(&self, _: i64, _: i64) -> Result<(), ScheduleError> {
        Err(ScheduleError::Unavailable)
    }

    async fn postpone(&self, _: i64, _: i64, _: Duration) -> Result<(), ScheduleError> {
        Err(ScheduleError::Unavailable)
    }

    async fn retry(&self, _: i64, _: i64, _: Duration, _: &str) -> Result<(), ScheduleError> {
        Err(ScheduleError::Unavailable)
    }

    async fn discard(&self, _: i64, _: i64) -> Result<(), ScheduleError> {
        Err(ScheduleError::Unavailable)
    }
}

impl Scheduler {
    /// The scheduler of `module` in `guild`; usable only if `granted` holds
    /// [`Capability::Schedule`].
    #[must_use]
    pub fn new(
        backend: Arc<dyn SchedulerBackend>,
        granted: &CapabilitySet,
        module: ModuleId,
        guild: GuildId,
    ) -> Self {
        Self {
            backend,
            granted: granted.contains(Capability::Schedule),
            module,
            guild,
        }
    }

    /// A scheduler that fails every call, for contexts that have none.
    #[must_use]
    pub fn unavailable(module: ModuleId, guild: GuildId) -> Self {
        Self {
            backend: Arc::new(NoScheduler),
            granted: true,
            module,
            guild,
        }
    }

    /// Starts describing a job: a `name` the module's handler knows, and the data it needs.
    pub fn job<T: Serialize>(
        &self,
        name: &str,
        payload: &T,
    ) -> Result<JobBuilder<'_>, ScheduleError> {
        if !self.granted {
            return Err(ScheduleError::NotGranted);
        }
        check_key(name).map_err(|_| {
            ScheduleError::InvalidName("a name is 1-128 printable ASCII characters".into())
        })?;
        let payload = serde_json::to_value(payload)
            .map_err(|error| ScheduleError::Payload(error.to_string()))?;
        if has_nul(&payload) {
            return Err(ScheduleError::Payload(
                "a text or a key of the payload holds a NUL character".into(),
            ));
        }
        let size = serde_json::to_vec(&payload).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_PAYLOAD_BYTES {
            return Err(ScheduleError::TooLarge {
                size,
                limit: MAX_PAYLOAD_BYTES,
            });
        }
        Ok(JobBuilder {
            scheduler: self,
            name: name.to_owned(),
            payload,
            key: None,
        })
    }

    /// Removes the job that was scheduled with `key`; whether there was one.
    pub async fn cancel(&self, key: &str) -> Result<bool, ScheduleError> {
        if !self.granted {
            return Err(ScheduleError::NotGranted);
        }
        check_key(key).map_err(|_| {
            ScheduleError::InvalidName("a key is 1-128 printable ASCII characters".into())
        })?;
        self.backend.cancel(&self.module, self.guild, key).await
    }
}

/// A job being described; finish it with [`Self::after`], [`Self::at`] or [`Self::every`].
#[derive(Debug)]
pub struct JobBuilder<'a> {
    scheduler: &'a Scheduler,
    name: String,
    payload: serde_json::Value,
    key: Option<String>,
}

impl JobBuilder<'_> {
    /// Gives the job a key: scheduling again with the same key replaces it, and
    /// [`Scheduler::cancel`] finds it.
    #[must_use]
    pub fn key(mut self, key: &str) -> Self {
        self.key = Some(key.to_owned());
        self
    }

    /// Runs once, this long from now.
    pub async fn after(self, delay: Duration) -> Result<(), ScheduleError> {
        self.submit(RunAt::After(delay), None).await
    }

    /// Runs once, at a time given in unix seconds.
    pub async fn at(self, unix_seconds: i64) -> Result<(), ScheduleError> {
        self.submit(RunAt::At(unix_seconds), None).await
    }

    /// Runs first after `first`, then every `every` after the planned time of the last run.
    pub async fn every(self, every: Duration, first: Duration) -> Result<(), ScheduleError> {
        if every < MIN_EVERY {
            return Err(ScheduleError::TooOften);
        }
        self.submit(RunAt::After(first), Some(every)).await
    }

    async fn submit(self, run_at: RunAt, every: Option<Duration>) -> Result<(), ScheduleError> {
        let in_range = match run_at {
            RunAt::After(delay) => delay <= MAX_DELAY,
            RunAt::At(at) => (0..=MAX_AT).contains(&at),
        } && every.is_none_or(|every| every <= MAX_DELAY);
        if !in_range {
            return Err(ScheduleError::OutOfRange);
        }
        if let Some(key) = &self.key {
            check_key(key).map_err(|_| {
                ScheduleError::InvalidName("a key is 1-128 printable ASCII characters".into())
            })?;
        }
        self.scheduler
            .backend
            .schedule(NewJob {
                module: self.scheduler.module.clone(),
                guild: self.scheduler.guild,
                name: self.name,
                payload: self.payload,
                run_at,
                every,
                key: self.key,
            })
            .await
            .map(drop)
    }
}

#[derive(Debug, Clone)]
struct Stored {
    job: NewJob,
    /// When it is handed out next, in unix seconds: a retry or a postponement moves this.
    due: i64,
    /// When it was planned to run; a repeating job counts its period from here, so a retry does
    /// not shift the period.
    planned: i64,
    /// Which hand-out holds it (0: none).
    token: i64,
    attempts: u32,
    leased_until: i64,
    failed: bool,
}

#[derive(Debug, Default)]
struct Jobs {
    next_id: i64,
    next_token: i64,
    by_id: BTreeMap<i64, Stored>,
}

type UnixClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// A [`SchedulerBackend`] in memory, with the same rules as the real one. For tests.
pub struct MemoryScheduler {
    clock: UnixClock,
    jobs: Mutex<Jobs>,
}

impl std::fmt::Debug for MemoryScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryScheduler").finish_non_exhaustive()
    }
}

impl Default for MemoryScheduler {
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

impl MemoryScheduler {
    /// A scheduler whose idea of the time (unix seconds) is `clock`.
    #[must_use]
    pub fn with_clock(clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        Self {
            clock: Arc::new(clock),
            jobs: Mutex::new(Jobs::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Jobs> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many jobs are waiting or failed, for tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().by_id.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The ids of the jobs that were given up on.
    #[must_use]
    pub fn failed(&self) -> Vec<i64> {
        self.lock()
            .by_id
            .iter()
            .filter(|(_, stored)| stored.failed)
            .map(|(id, _)| *id)
            .collect()
    }
}

/// PostgreSQL cannot keep a NUL character in JSON; the memory scheduler refuses it as well, so a
/// test does not pass with what the real one would reject.
fn has_nul(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains('\0'),
        serde_json::Value::Array(items) => items.iter().any(has_nul),
        serde_json::Value::Object(map) => map
            .iter()
            .any(|(key, item)| key.contains('\0') || has_nul(item)),
        _ => false,
    }
}

/// The first planned time of a repeating job after `due` that is still ahead of `now`: runs
/// missed while the bot was down are skipped, not made up for.
#[must_use]
pub fn next_run(due: i64, every: i64, now: i64) -> i64 {
    let every = every.max(1);
    let missed = (now.saturating_sub(due) / every).max(0);
    due.saturating_add(every.saturating_mul(missed.saturating_add(1)))
}

fn seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

#[async_trait]
impl SchedulerBackend for MemoryScheduler {
    async fn schedule(&self, job: NewJob) -> Result<i64, ScheduleError> {
        let now = (self.clock)();
        let due = match job.run_at {
            RunAt::After(delay) => now.saturating_add(seconds(delay)),
            RunAt::At(at) => at,
        };
        let mut jobs = self.lock();
        let existing = job.key.as_ref().and_then(|key| {
            jobs.by_id
                .iter()
                .find(|(_, stored)| {
                    stored.job.module == job.module
                        && stored.job.guild == job.guild
                        && stored.job.key.as_ref() == Some(key)
                })
                .map(|(id, _)| *id)
        });
        let fresh = Stored {
            job: job.clone(),
            due,
            planned: due,
            token: 0,
            attempts: 0,
            leased_until: 0,
            failed: false,
        };
        if let Some(id) = existing {
            jobs.by_id.insert(id, fresh);
            return Ok(id);
        }
        let waiting = jobs
            .by_id
            .values()
            .filter(|stored| stored.job.module == job.module && stored.job.guild == job.guild)
            .count();
        if waiting >= MAX_JOBS {
            return Err(ScheduleError::QuotaExceeded { limit: MAX_JOBS });
        }
        jobs.next_id += 1;
        let id = jobs.next_id;
        jobs.by_id.insert(id, fresh);
        Ok(id)
    }

    async fn cancel(
        &self,
        module: &ModuleId,
        guild: GuildId,
        key: &str,
    ) -> Result<bool, ScheduleError> {
        let mut jobs = self.lock();
        let found = jobs
            .by_id
            .iter()
            .find(|(_, stored)| {
                stored.job.module == *module
                    && stored.job.guild == guild
                    && stored.job.key.as_deref() == Some(key)
            })
            .map(|(id, _)| *id);
        Ok(found.is_some_and(|id| jobs.by_id.remove(&id).is_some()))
    }

    async fn claim_due(
        &self,
        limit: usize,
        lease: Duration,
    ) -> Result<Vec<ScheduledJob>, ScheduleError> {
        let now = (self.clock)();
        let mut jobs = self.lock();
        // A job that was taken over and over without ever being reported (the bot keeps stopping
        // while it runs) is given up on too.
        for stored in jobs.by_id.values_mut() {
            if stored.attempts >= MAX_ATTEMPTS && stored.leased_until <= now {
                stored.failed = true;
            }
        }
        let mut due: Vec<(i64, i64)> = jobs
            .by_id
            .iter()
            .filter(|(_, stored)| {
                !stored.failed
                    && stored.attempts < MAX_ATTEMPTS
                    && stored.due <= now
                    && stored.leased_until <= now
            })
            .map(|(id, stored)| (stored.due, *id))
            .collect();
        due.sort_unstable();
        due.truncate(limit);
        let mut claimed = Vec::new();
        for (_, id) in due {
            jobs.next_token += 1;
            let token = jobs.next_token;
            if let Some(stored) = jobs.by_id.get_mut(&id) {
                stored.attempts += 1;
                stored.token = token;
                stored.leased_until = now.saturating_add(seconds(lease));
                claimed.push(ScheduledJob {
                    id,
                    guild_id: stored.job.guild,
                    module: stored.job.module.clone(),
                    name: stored.job.name.clone(),
                    payload: stored.job.payload.clone(),
                    attempt: stored.attempts,
                    token,
                });
            }
        }
        Ok(claimed)
    }

    async fn complete(&self, id: i64, token: i64) -> Result<(), ScheduleError> {
        let now = (self.clock)();
        let mut jobs = self.lock();
        let Some(stored) = jobs
            .by_id
            .get_mut(&id)
            .filter(|stored| stored.token == token)
        else {
            return Ok(());
        };
        if let Some(every) = stored.job.every {
            stored.planned = next_run(stored.planned, seconds(every), now);
            stored.due = stored.planned;
            stored.attempts = 0;
            stored.leased_until = 0;
            stored.token = 0;
        } else {
            jobs.by_id.remove(&id);
        }
        Ok(())
    }

    async fn postpone(&self, id: i64, token: i64, after: Duration) -> Result<(), ScheduleError> {
        let now = (self.clock)();
        if let Some(stored) = self
            .lock()
            .by_id
            .get_mut(&id)
            .filter(|stored| stored.token == token)
        {
            stored.attempts = stored.attempts.saturating_sub(1);
            stored.due = now.saturating_add(seconds(after));
            stored.leased_until = 0;
            stored.token = 0;
        }
        Ok(())
    }

    async fn retry(
        &self,
        id: i64,
        token: i64,
        after: Duration,
        _error: &str,
    ) -> Result<(), ScheduleError> {
        let now = (self.clock)();
        if let Some(stored) = self
            .lock()
            .by_id
            .get_mut(&id)
            .filter(|stored| stored.token == token)
        {
            stored.failed = stored.attempts >= MAX_ATTEMPTS;
            stored.due = now.saturating_add(seconds(after));
            stored.leased_until = 0;
            stored.token = 0;
        }
        Ok(())
    }

    async fn discard(&self, id: i64, token: i64) -> Result<(), ScheduleError> {
        let mut jobs = self.lock();
        if jobs
            .by_id
            .get(&id)
            .is_some_and(|stored| stored.token == token)
        {
            jobs.by_id.remove(&id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use serde_json::json;
    use twilight_model::id::Id;

    use super::*;

    struct Fixture {
        backend: Arc<MemoryScheduler>,
        now: Arc<AtomicI64>,
    }

    fn fixture() -> Fixture {
        let now = Arc::new(AtomicI64::new(1_700_000_000));
        let clock = Arc::clone(&now);
        Fixture {
            backend: Arc::new(MemoryScheduler::with_clock(move || {
                clock.load(Ordering::SeqCst)
            })),
            now,
        }
    }

    impl Fixture {
        fn scheduler(&self, module: &str, guild: u64) -> Scheduler {
            let granted: CapabilitySet = [Capability::Schedule].into_iter().collect();
            Scheduler::new(
                Arc::clone(&self.backend) as Arc<dyn SchedulerBackend>,
                &granted,
                ModuleId::new(module).unwrap(),
                Id::new(guild),
            )
        }

        fn advance(&self, seconds: i64) {
            self.now.fetch_add(seconds, Ordering::SeqCst);
        }

        async fn due(&self) -> Vec<ScheduledJob> {
            self.backend
                .claim_due(100, Duration::from_secs(60))
                .await
                .unwrap()
        }
    }

    #[test]
    fn the_wait_grows_and_stops_growing() {
        assert_eq!(backoff(1), Duration::from_secs(5));
        assert_eq!(backoff(2), Duration::from_secs(10));
        assert_eq!(backoff(3), Duration::from_secs(20));
        assert_eq!(backoff(30), Duration::from_secs(3600));
        assert_eq!(backoff(0), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_job_is_handed_out_once_it_is_due_and_not_again_while_it_runs() {
        let f = fixture();
        let scheduler = f.scheduler("warn", 1);
        scheduler
            .job("expire", &json!({ "user": 7 }))
            .unwrap()
            .after(Duration::from_secs(60))
            .await
            .unwrap();
        assert!(f.due().await.is_empty());
        f.advance(59);
        assert!(f.due().await.is_empty());
        f.advance(1);
        let jobs = f.due().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "expire");
        assert_eq!(jobs[0].payload, json!({ "user": 7 }));
        assert_eq!(jobs[0].attempt, 1);
        // Being run: not handed out again, until the lease is over (the bot died meanwhile).
        assert!(f.due().await.is_empty());
        f.advance(61);
        let again = f.due().await.remove(0);
        assert_eq!(again.attempt, 2);
        // The first hand-out is over; only the second can say how it went.
        f.backend.complete(jobs[0].id, jobs[0].token).await.unwrap();
        assert!(!f.backend.is_empty());
        f.backend.complete(again.id, again.token).await.unwrap();
        assert!(f.backend.is_empty());
    }

    #[tokio::test]
    async fn a_failed_job_comes_back_after_a_pause_and_is_given_up_on_after_enough_tries() {
        let f = fixture();
        f.scheduler("warn", 1)
            .job("x", &1)
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        let mut attempt = 0;
        loop {
            let jobs = f.due().await;
            let Some(job) = jobs.first() else { break };
            attempt = job.attempt;
            f.backend
                .retry(job.id, job.token, backoff(job.attempt), "boom")
                .await
                .unwrap();
            f.advance(3601);
        }
        assert_eq!(attempt, MAX_ATTEMPTS);
        assert_eq!(f.backend.failed().len(), 1);
        assert!(f.due().await.is_empty());
    }

    #[tokio::test]
    async fn a_repeating_job_is_set_for_its_next_time_after_each_run() {
        let f = fixture();
        f.scheduler("warn", 1)
            .job("tick", &())
            .unwrap()
            .every(Duration::from_secs(60), Duration::from_secs(10))
            .await
            .unwrap();
        f.advance(10);
        let job = f.due().await.remove(0);
        f.backend.complete(job.id, job.token).await.unwrap();
        assert!(f.due().await.is_empty());
        f.advance(59);
        assert!(f.due().await.is_empty());
        f.advance(1);
        assert_eq!(
            f.due().await[0].attempt,
            1,
            "a new run starts counting again"
        );
        // A long stop does not make it catch up with every run it missed.
        f.advance(10_000);
        let job = f.due().await.remove(0);
        f.backend.complete(job.id, job.token).await.unwrap();
        assert!(f.due().await.is_empty());
    }

    #[tokio::test]
    async fn a_job_that_keeps_getting_lost_is_given_up_on() {
        let f = fixture();
        f.scheduler("warn", 1)
            .job("x", &1)
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        // Taken, never reported: the lease runs out each time.
        for attempt in 1..=MAX_ATTEMPTS {
            assert_eq!(f.due().await[0].attempt, attempt);
            f.advance(61);
        }
        assert!(f.due().await.is_empty());
        assert_eq!(f.backend.failed().len(), 1);
    }

    #[test]
    fn the_next_run_skips_the_ones_that_were_missed() {
        assert_eq!(next_run(100, 60, 100), 160);
        assert_eq!(next_run(100, 60, 159), 160);
        assert_eq!(next_run(100, 60, 160), 220);
        assert_eq!(next_run(100, 60, 10_000), 10_060);
        assert_eq!(next_run(100, 60, 0), 160, "a clock that went back");
    }

    #[tokio::test]
    async fn a_postponed_job_keeps_its_count_of_tries() {
        let f = fixture();
        f.scheduler("warn", 1)
            .job("x", &1)
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
        let job = f.due().await.remove(0);
        f.backend
            .postpone(job.id, job.token, Duration::from_secs(30))
            .await
            .unwrap();
        assert!(f.due().await.is_empty());
        f.advance(30);
        assert_eq!(
            f.due().await[0].attempt,
            1,
            "the postponed try did not count"
        );
    }

    #[tokio::test]
    async fn a_time_that_cannot_be_kept_is_refused() {
        let f = fixture();
        let scheduler = f.scheduler("warn", 1);
        let far = MAX_DELAY + Duration::from_secs(1);
        assert_eq!(
            scheduler.job("x", &1).unwrap().after(far).await,
            Err(ScheduleError::OutOfRange)
        );
        assert_eq!(
            scheduler.job("x", &1).unwrap().at(-1).await,
            Err(ScheduleError::OutOfRange)
        );
        assert_eq!(
            scheduler.job("x", &1).unwrap().at(MAX_AT + 1).await,
            Err(ScheduleError::OutOfRange)
        );
        assert_eq!(
            scheduler
                .job("x", &1)
                .unwrap()
                .every(far, Duration::ZERO)
                .await,
            Err(ScheduleError::OutOfRange)
        );
        scheduler.job("x", &1).unwrap().at(MAX_AT).await.unwrap();
        scheduler
            .job("x", &1)
            .unwrap()
            .after(MAX_DELAY)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_key_replaces_and_cancels_and_is_the_modules_own() {
        let f = fixture();
        let warn = f.scheduler("warn", 1);
        warn.job("unmute", &1)
            .unwrap()
            .key("user:7")
            .after(Duration::from_secs(100))
            .await
            .unwrap();
        warn.job("unmute", &2)
            .unwrap()
            .key("user:7")
            .after(Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(f.backend.len(), 1);
        f.advance(10);
        assert_eq!(f.due().await[0].payload, json!(2));

        // Others' keys are their own, and cancelling finds only the module's.
        f.scheduler("mute", 1)
            .job("x", &0)
            .unwrap()
            .key("user:7")
            .after(Duration::from_secs(5))
            .await
            .unwrap();
        f.scheduler("warn", 2)
            .job("x", &0)
            .unwrap()
            .key("user:7")
            .after(Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(f.backend.len(), 3);
        assert!(warn.cancel("user:7").await.unwrap());
        assert!(!warn.cancel("user:7").await.unwrap());
        assert_eq!(f.backend.len(), 2);
    }

    #[tokio::test]
    async fn names_payloads_the_capability_and_the_quota_are_checked() {
        let f = fixture();
        let scheduler = f.scheduler("warn", 1);
        for name in ["", "has space", &"n".repeat(129)] {
            assert!(
                matches!(scheduler.job(name, &1), Err(ScheduleError::InvalidName(_))),
                "{name:?}"
            );
        }
        let big = "x".repeat(MAX_PAYLOAD_BYTES);
        assert!(matches!(
            scheduler.job("big", &big),
            Err(ScheduleError::TooLarge { .. })
        ));
        let builder = scheduler.job("fast", &1).unwrap();
        assert_eq!(
            builder.every(Duration::from_secs(1), Duration::ZERO).await,
            Err(ScheduleError::TooOften)
        );
        let granted_nothing = Scheduler::new(
            Arc::clone(&f.backend) as Arc<dyn SchedulerBackend>,
            &CapabilitySet::new(),
            ModuleId::new("warn").unwrap(),
            Id::new(1),
        );
        assert!(matches!(
            granted_nothing.job("x", &1),
            Err(ScheduleError::NotGranted)
        ));
        assert_eq!(
            granted_nothing.cancel("k").await,
            Err(ScheduleError::NotGranted)
        );
        let nowhere = Scheduler::unavailable(ModuleId::new("warn").unwrap(), Id::new(1));
        assert_eq!(
            nowhere.job("x", &1).unwrap().after(Duration::ZERO).await,
            Err(ScheduleError::Unavailable)
        );

        for n in 0..MAX_JOBS {
            scheduler
                .job("x", &n)
                .unwrap()
                .after(Duration::from_secs(5))
                .await
                .unwrap();
        }
        assert_eq!(
            scheduler.job("x", &0).unwrap().after(Duration::ZERO).await,
            Err(ScheduleError::QuotaExceeded { limit: MAX_JOBS })
        );
        // Another guild has its own room; a key that exists may still be replaced.
        f.scheduler("warn", 2)
            .job("x", &0)
            .unwrap()
            .after(Duration::ZERO)
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn a_report_about_a_job_that_was_replaced_or_handed_out_again_changes_nothing() {
        let f = fixture();
        let warn = f.scheduler("warn", 1);
        warn.job("cleanup", &1)
            .unwrap()
            .key("c")
            .after(Duration::ZERO)
            .await
            .unwrap();
        let old = f.due().await.remove(0);
        // The handler sets the next one under the same key, then the old run is reported done.
        warn.job("cleanup", &2)
            .unwrap()
            .key("c")
            .after(Duration::from_secs(600))
            .await
            .unwrap();
        f.backend.complete(old.id, old.token).await.unwrap();
        f.backend
            .retry(old.id, old.token, Duration::ZERO, "x")
            .await
            .unwrap();
        f.backend.discard(old.id, old.token).await.unwrap();
        assert_eq!(f.backend.len(), 1, "the new one is still there");
        f.advance(600);
        assert_eq!(
            f.due().await[0].payload,
            json!(2),
            "and still due at its time"
        );

        // Handed out again after the lease: the first hand-out can no longer report.
        let first = f.due().await;
        assert!(first.is_empty());
        f.advance(61);
        let second = f.due().await.remove(0);
        f.advance(1);
        f.backend
            .complete(second.id, second.token - 1)
            .await
            .unwrap();
        assert_eq!(f.backend.len(), 1);
        f.backend.complete(second.id, second.token).await.unwrap();
        assert!(f.backend.is_empty());
    }

    #[tokio::test]
    async fn a_retry_does_not_move_the_period_of_a_repeating_job() {
        let f = fixture();
        f.scheduler("warn", 1)
            .job("tick", &())
            .unwrap()
            .every(Duration::from_secs(60), Duration::ZERO)
            .await
            .unwrap();
        let job = f.due().await.remove(0);
        // It fails and is tried again five seconds later; the next run is still a minute after
        // the planned time, not after the retry.
        f.backend
            .retry(job.id, job.token, backoff(1), "boom")
            .await
            .unwrap();
        f.advance(5);
        let again = f.due().await.remove(0);
        f.backend.complete(again.id, again.token).await.unwrap();
        f.advance(54);
        assert!(f.due().await.is_empty());
        f.advance(1);
        assert_eq!(
            f.due().await.len(),
            1,
            "a minute after the first planned time"
        );
    }

    #[test]
    fn a_nul_character_in_a_payload_is_refused_as_the_database_would() {
        let f = fixture();
        let warn = f.scheduler("warn", 1);
        assert!(matches!(
            warn.job("x", &"a\0b"),
            Err(ScheduleError::Payload(_))
        ));
        assert!(matches!(
            warn.job("x", &json!({ "k\0": 1 })),
            Err(ScheduleError::Payload(_))
        ));
        assert!(
            warn.job("x", &"a\\u0000b").is_ok(),
            "backslash and text only"
        );
    }
}
