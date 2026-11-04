//! Runtime adapter test fixtures. A factory maps each case name to a guest program.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use closure_core::module::ModuleRuntime;
use serde_json::{Value, json};

use crate::RuntimeFactory;
use crate::runtime::{Budget, HostApi};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConformanceCase {
    Echo,
    HostCalls,
    InfiniteLoop,
    MemoryGrowth,
    Trap,
    HugeOutput,
}

impl ConformanceCase {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Echo => "echo",
            Self::HostCalls => "host_calls",
            Self::InfiniteLoop => "infinite_loop",
            Self::MemoryGrowth => "memory_growth",
            Self::Trap => "trap",
            Self::HugeOutput => "huge_output",
        }
    }
}

#[derive(Default)]
struct TestHost(Mutex<BTreeMap<String, Value>>);
impl HostApi for TestHost {
    fn execute(&self, action: Value) -> Result<Value, String> {
        Ok(action)
    }
    fn get(&self, key: &str) -> Result<Value, String> {
        Ok(self
            .0
            .lock()
            .map_err(|e| e.to_string())?
            .get(key)
            .cloned()
            .unwrap_or(Value::Null))
    }
    fn set(&self, key: &str, value: Value) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .insert(key.into(), value);
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<bool, String> {
        Ok(self
            .0
            .lock()
            .map_err(|e| e.to_string())?
            .remove(key)
            .is_some())
    }
    fn list(&self, prefix: &str, _: usize) -> Result<Value, String> {
        Ok(json!(
            self.0
                .lock()
                .map_err(|e| e.to_string())?
                .keys()
                .filter(|k| k.starts_with(prefix))
                .collect::<Vec<_>>()
        ))
    }
}

/// Returns failed cases; the factory receives each case name as its entry fixture.
pub fn run_all(factory: &dyn RuntimeFactory) -> Vec<ConformanceCase> {
    run_all_for(factory, ModuleRuntime::Wasm)
}

/// Runs the same fixture contract for a specific runtime kind.
pub fn run_all_for(factory: &dyn RuntimeFactory, kind: ModuleRuntime) -> Vec<ConformanceCase> {
    [
        ConformanceCase::Echo,
        ConformanceCase::HostCalls,
        ConformanceCase::InfiniteLoop,
        ConformanceCase::MemoryGrowth,
        ConformanceCase::Trap,
        ConformanceCase::HugeOutput,
    ]
    .into_iter()
    .filter(|case| !run_case(factory, kind, *case))
    .collect()
}

fn run_case(factory: &dyn RuntimeFactory, kind: ModuleRuntime, case: ConformanceCase) -> bool {
    let Ok(Some(runtime)) = factory.create(kind, case.name().as_bytes()) else {
        return false;
    };
    let (tx, rx) = mpsc::sync_channel(1);
    let host = Arc::new(TestHost::default());
    let input = json!({"type":"config_changed", "data":{"guild_id":"1"}});
    let thread_host = Arc::clone(&host);
    let budget = Budget {
        cpu_ms: 20,
        memory_bytes: 1024 * 1024,
        output_bytes: crate::Limits::default().output_bytes,
        deadline: Instant::now() + Duration::from_millis(100),
    };
    if std::thread::Builder::new()
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if case == ConformanceCase::HugeOutput {
                    let executor = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                        .map_err(|e| e.to_string())?;
                    let pool = crate::runtime::Pool::new(1, 1)?;
                    executor
                        .block_on(pool.call(runtime, thread_host, input, crate::Limits::default()))
                        .map_err(|error| error.to_string())
                } else {
                    runtime.call(input, thread_host, budget)
                }
            }));
            let _ = tx.send(result);
        })
        .is_err()
    {
        return false;
    }
    let Ok(result) = rx.recv_timeout(Duration::from_millis(200)) else {
        return false;
    };
    match case {
        ConformanceCase::Echo => {
            matches!(result, Ok(Ok(value)) if value == json!([{"type":"get_guild", "guild_id":"1"}]))
        }
        ConformanceCase::HostCalls => {
            matches!(result, Ok(Ok(value)) if value == json!({"action":"ok","stored":"ok"}))
                && host.get("key").ok() == Some(json!("ok"))
        }
        ConformanceCase::InfiniteLoop | ConformanceCase::MemoryGrowth => {
            matches!(result, Ok(Err(_)))
        }
        ConformanceCase::Trap => matches!(result, Ok(Err(_)) | Err(_)),
        ConformanceCase::HugeOutput => {
            matches!(result, Ok(Err(error)) if error.contains("output exceeds the limit"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{FakeRuntime, ScriptRuntime};

    struct FakeFactory;
    impl RuntimeFactory for FakeFactory {
        fn create(
            &self,
            _: ModuleRuntime,
            entry: &[u8],
        ) -> Result<Option<Arc<dyn ScriptRuntime>>, String> {
            let case = String::from_utf8(entry.to_vec()).map_err(|e| e.to_string())?;
            Ok(Some(Arc::new(FakeRuntime::new(
                move |input, host, _| match case.as_str() {
                    "echo"
                        if input["type"] == "config_changed"
                            && input["data"]["guild_id"] == "1" =>
                    {
                        Ok(json!([{"type":"get_guild", "guild_id":"1"}]))
                    }
                    "host_calls" => {
                        host.set("key", json!("ok"))?;
                        let stored = host.get("key")?;
                        let action = host.execute(json!("ok"))?;
                        Ok(json!({"action":action,"stored":stored}))
                    }
                    "trap" => Err("rejected".into()),
                    "huge_output" => Ok(json!([
                        "x".repeat(crate::Limits::default().output_bytes + 1)
                    ])),
                    _ => Ok(json!([])),
                },
            ))))
        }
    }

    #[test]
    fn fake_runtime_exposes_missing_enforcement() {
        assert_eq!(
            run_all(&FakeFactory),
            [ConformanceCase::InfiniteLoop, ConformanceCase::MemoryGrowth]
        );
    }
}
