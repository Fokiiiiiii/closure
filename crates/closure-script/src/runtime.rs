//! The synchronous runtime contract and a bounded, isolated worker pool.

use std::collections::VecDeque;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use serde_json::Value;
use tokio::sync::oneshot;

use crate::Limits;

/// A runtime must enforce both the CPU and memory budget and stop by the deadline.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub cpu_ms: u64,
    pub memory_bytes: usize,
    pub output_bytes: usize,
    pub deadline: Instant,
}

pub(crate) fn json_size(value: &Value, limit: usize) -> Result<usize, String> {
    struct Counter {
        size: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.size = self.size.saturating_add(bytes.len());
            if self.size > self.limit {
                return Err(std::io::Error::other("script output exceeds the limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { size: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|e| e.to_string())?;
    Ok(counter.size)
}

/// Synchronous access to the scoped host services.
pub trait HostApi: Send + Sync {
    fn execute(&self, action: Value) -> Result<Value, String>;
    fn get(&self, key: &str) -> Result<Value, String>;
    fn set(&self, key: &str, value: Value) -> Result<(), String>;
    fn delete(&self, key: &str) -> Result<bool, String>;
    fn list(&self, prefix: &str, limit: usize) -> Result<Value, String>;
    fn limit_failure(&self) -> Option<String> {
        None
    }
}

/// Implementations must interrupt their own execution when `budget` is exhausted.
pub trait ScriptRuntime: Send + Sync + 'static {
    fn call(&self, input: Value, host: Arc<dyn HostApi>, budget: Budget) -> Result<Value, String>;
}

type Callback = dyn Fn(Value, &dyn HostApi, Budget) -> Result<Value, String> + Send + Sync;

/// An injectable runtime for tests and adapter development.
pub struct FakeRuntime(Arc<Callback>);

impl fmt::Debug for FakeRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeRuntime").finish_non_exhaustive()
    }
}

impl FakeRuntime {
    pub fn new(
        callback: impl Fn(Value, &dyn HostApi, Budget) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(callback))
    }
}

impl ScriptRuntime for FakeRuntime {
    fn call(&self, input: Value, host: Arc<dyn HostApi>, budget: Budget) -> Result<Value, String> {
        (self.0)(input, host.as_ref(), budget)
    }
}

type Job = Box<dyn FnOnce() + Send>;

#[derive(Debug)]
pub enum CallError {
    Busy,
    /// The script failed; only the guild that ran it is affected.
    Failed(String),
    /// The pool failed (a lost worker, a missed deadline); every script on it is affected.
    Runtime(String),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("script queue is full"),
            Self::Failed(error) | Self::Runtime(error) => f.write_str(error),
        }
    }
}
impl From<String> for CallError {
    fn from(error: String) -> Self {
        Self::Failed(error)
    }
}

/// A bounded pool owned by one script; it never uses Tokio's shared blocking pool.
pub struct Pool {
    queue: mpsc::SyncSender<Job>,
    breaker: Arc<Breaker>,
}

/// Permanent disabled state and the recent failure count for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptStatus {
    pub disabled: bool,
    pub recent_failures: usize,
}

/// How long an open breaker stays open before the script gets another chance.
const BREAKER_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);
/// After that chance, a failure within this long opens the breaker again at once.
const BREAKER_PROBE: std::time::Duration = std::time::Duration::from_secs(60);

/// Tracks failures across calls. An open breaker closes by itself after a while, so a script that
/// failed in a bad hour is not off until the next restart; a failure soon after reopens it.
#[derive(Debug)]
pub struct Breaker {
    state: Mutex<BreakerState>,
    cooldown: std::time::Duration,
}

impl Default for Breaker {
    fn default() -> Self {
        Self {
            state: Mutex::new(BreakerState::default()),
            cooldown: BREAKER_COOLDOWN,
        }
    }
}

#[derive(Debug, Default)]
struct BreakerState {
    failures: VecDeque<Instant>,
    disabled: bool,
    opened_at: Option<Instant>,
    probing_until: Option<Instant>,
}

impl BreakerState {
    /// Closes the breaker when its time is up.
    fn refresh(&mut self, now: Instant, cooldown: std::time::Duration) {
        if self.disabled
            && self
                .opened_at
                .is_some_and(|at| now.duration_since(at) >= cooldown)
        {
            self.disabled = false;
            self.opened_at = None;
            self.failures.clear();
            self.probing_until = Some(now + BREAKER_PROBE);
        }
    }
}

impl Breaker {
    #[must_use]
    pub fn status(&self) -> ScriptStatus {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.refresh(Instant::now(), self.cooldown);
        ScriptStatus {
            disabled: state.disabled,
            recent_failures: state.failures.len(),
        }
    }

    /// Returns true exactly once, when the breaker opens.
    pub fn record_failure(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        state.refresh(now, self.cooldown);
        while state
            .failures
            .front()
            .is_some_and(|time| now.duration_since(*time) >= std::time::Duration::from_secs(60))
        {
            state.failures.pop_front();
        }
        if state.disabled {
            return false;
        }
        state.failures.push_back(now);
        let probing = state.probing_until.is_some_and(|until| now < until);
        if probing || state.failures.len() >= 5 {
            state.disabled = true;
            state.opened_at = Some(now);
            state.probing_until = None;
            return true;
        }
        false
    }
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool").finish_non_exhaustive()
    }
}

impl Pool {
    pub fn new(workers: usize, queue: usize) -> Result<Self, String> {
        if workers == 0 || workers > Limits::default().workers || queue == 0 {
            return Err("invalid script pool size".into());
        }
        let (sender, receiver) = mpsc::sync_channel::<Job>(queue);
        let receiver = Arc::new(Mutex::new(receiver));
        for _ in 0..workers {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name("closure-script".into())
                .spawn(move || {
                    loop {
                        let job = match receiver.lock() {
                            Ok(queue) => queue.recv(),
                            Err(_) => return,
                        };
                        match job {
                            Ok(job) => job(),
                            Err(_) => return,
                        }
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(Self {
            queue: sender,
            breaker: Arc::new(Breaker::default()),
        })
    }

    #[must_use]
    pub fn status(&self) -> ScriptStatus {
        self.breaker.status()
    }

    pub(crate) fn breaker(&self) -> Arc<Breaker> {
        Arc::clone(&self.breaker)
    }

    pub(crate) fn record_failure(&self) {
        if self.breaker.record_failure() {
            tracing::error!("script disabled after repeated failures");
        }
    }

    pub async fn call(
        &self,
        runtime: Arc<dyn ScriptRuntime>,
        host: Arc<dyn HostApi>,
        input: Value,
        limits: Limits,
    ) -> Result<Value, CallError> {
        if self.status().disabled {
            return Err(CallError::Runtime("script is disabled".into()));
        }
        let result = self.call_inner(runtime, host, input, limits).await;
        if matches!(result, Err(CallError::Runtime(_))) {
            self.record_failure();
        }
        result
    }

    async fn call_inner(
        &self,
        runtime: Arc<dyn ScriptRuntime>,
        host: Arc<dyn HostApi>,
        input: Value,
        limits: Limits,
    ) -> Result<Value, CallError> {
        let limits = limits.bounded(Limits::default()).map_err(str::to_owned)?;
        let budget = Budget {
            cpu_ms: limits.cpu_ms,
            memory_bytes: limits.memory_bytes,
            output_bytes: limits.output_bytes,
            deadline: Instant::now() + limits.wall_time(),
        };
        let (send, receive) = oneshot::channel::<Result<Value, CallError>>();
        self.queue
            .try_send(Box::new(move || {
                let result = catch_unwind(AssertUnwindSafe(|| -> Result<Value, CallError> {
                    if Instant::now() >= budget.deadline {
                        return Err(CallError::Runtime("script deadline exceeded".into()));
                    }
                    let result = runtime
                        .call(input, Arc::clone(&host), budget)
                        .map_err(CallError::Failed);
                    match host.limit_failure() {
                        Some(error) => Err(CallError::Failed(error)),
                        None => result,
                    }
                }))
                .unwrap_or_else(|_| Err(CallError::Failed("script panicked".into())));
                let _ = send.send(result);
            }))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => CallError::Busy,
                mpsc::TrySendError::Disconnected(_) => {
                    CallError::Runtime("script queue is closed".into())
                }
            })?;
        let result =
            tokio::time::timeout_at(tokio::time::Instant::from_std(budget.deadline), receive)
                .await
                .map_err(|_| CallError::Runtime("script deadline exceeded".into()))?
                .map_err(|_| CallError::Runtime("script worker stopped".into()))??;
        let actions = result
            .as_array()
            .ok_or_else(|| "script output is not an array".to_owned())?;
        if actions.len() > limits.actions || json_size(&result, limits.output_bytes).is_err() {
            return Err(CallError::Failed("script output exceeds the limit".into()));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Host;
    impl HostApi for Host {
        fn execute(&self, _: Value) -> Result<Value, String> {
            Err("denied".into())
        }
        fn get(&self, _: &str) -> Result<Value, String> {
            Err("denied".into())
        }
        fn set(&self, _: &str, _: Value) -> Result<(), String> {
            Err("denied".into())
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            Err("denied".into())
        }
        fn list(&self, _: &str, _: usize) -> Result<Value, String> {
            Err("denied".into())
        }
    }

    #[tokio::test]
    async fn output_limits_and_panic_are_isolated() {
        let pool = Pool::new(1, 2).unwrap();
        let host: Arc<dyn HostApi> = Arc::new(Host);
        let limits = Limits::default();
        let panic = Arc::new(FakeRuntime::new(|_, _, _| panic!("trap")));
        assert!(
            pool.call(panic, Arc::clone(&host), Value::Null, limits)
                .await
                .is_err()
        );
        let oversized = Arc::new(FakeRuntime::new(|_, _, _| {
            Ok(serde_json::json!(vec![0; 26]))
        }));
        assert!(
            pool.call(oversized, Arc::clone(&host), Value::Null, limits)
                .await
                .is_err()
        );
        let large = Arc::new(FakeRuntime::new(|_, _, _| {
            Ok(serde_json::json!(["a".repeat(300_000)]))
        }));
        assert!(
            pool.call(large, Arc::clone(&host), Value::Null, limits)
                .await
                .is_err()
        );
        let echo = Arc::new(FakeRuntime::new(|value, _, _| {
            Ok(serde_json::json!([value]))
        }));
        assert_eq!(
            pool.call(echo, host, serde_json::json!(42), limits)
                .await
                .unwrap(),
            serde_json::json!([42])
        );
    }

    #[tokio::test]
    async fn deadline_is_observed() {
        let pool = Pool::new(1, 1).unwrap();
        let runtime = Arc::new(FakeRuntime::new(|_, _, budget| {
            while Instant::now() < budget.deadline {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(5));
            Err("interrupted".into())
        }));
        let limits = Limits {
            wall_ms: 10,
            ..Limits::default()
        };
        let start = Instant::now();
        assert!(
            pool.call(runtime, Arc::new(Host), Value::Null, limits)
                .await
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(pool.status().recent_failures, 1);
    }

    #[test]
    fn breaker_opens_once_and_a_new_instance_is_enabled() {
        let breaker = Breaker::default();
        for _ in 0..4 {
            assert!(!breaker.record_failure());
        }
        assert!(!breaker.status().disabled);
        assert!(breaker.record_failure());
        assert!(breaker.status().disabled);
        assert!(!breaker.record_failure());
        assert!(!Breaker::default().status().disabled);
    }

    #[test]
    fn an_open_breaker_closes_after_its_cooldown_and_a_failure_soon_after_opens_it_again() {
        let breaker = Breaker {
            state: Mutex::new(BreakerState::default()),
            cooldown: std::time::Duration::ZERO,
        };
        for _ in 0..4 {
            assert!(!breaker.record_failure());
        }
        assert!(breaker.record_failure());
        // The cooldown is over at once: the next look finds it closed.
        assert!(!breaker.status().disabled);
        // One failure in the probing time is enough to open it again.
        assert!(breaker.record_failure());
    }

    #[tokio::test]
    async fn disabled_pool_refuses_further_calls() {
        let pool = Pool::new(1, 1).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let runtime: Arc<dyn ScriptRuntime> = Arc::new(FakeRuntime::new(move |_, _, budget| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            while Instant::now() < budget.deadline {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(5));
            Err("interrupted".into())
        }));
        let limits = Limits {
            wall_ms: 10,
            ..Limits::default()
        };
        for _ in 0..6 {
            assert!(
                pool.call(Arc::clone(&runtime), Arc::new(Host), Value::Null, limits)
                    .await
                    .is_err()
            );
        }
        assert!(pool.status().disabled);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn full_queue_rejects_calls() {
        let pool = Pool::new(1, 1).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        assert!(
            pool.queue
                .try_send(Box::new(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                }))
                .is_ok()
        );
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let (drained_tx, drained_rx) = mpsc::channel();
        assert!(
            pool.queue
                .try_send(Box::new(move || {
                    drained_tx.send(()).unwrap();
                }))
                .is_ok()
        );
        let runtime = Arc::new(FakeRuntime::new(|_, _, _| Ok(serde_json::json!([]))));
        for _ in 0..6 {
            let result = pool
                .call(
                    runtime.clone(),
                    Arc::new(Host),
                    Value::Null,
                    Limits::default(),
                )
                .await;
            assert!(result.unwrap_err().to_string().contains("queue is full"));
        }
        assert!(!pool.status().disabled);
        assert_eq!(pool.status().recent_failures, 0);
        release_tx.send(()).unwrap();
        drained_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            pool.call(runtime, Arc::new(Host), Value::Null, Limits::default())
                .await
                .is_ok()
        );
    }
}
