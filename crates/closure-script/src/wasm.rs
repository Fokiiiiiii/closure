use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wasmtime::{
    Caller, Config, Engine, ExternType, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
    UpdateDeadline, ValType,
};

use crate::runtime::{Budget, HostApi, ScriptRuntime};

#[derive(Debug)]
pub struct WasmRuntime {
    engine: Arc<Engine>,
    module: Module,
}

struct State {
    host: Arc<dyn HostApi>,
    limits: StoreLimits,
    budget: Budget,
    started: Instant,
    host_time: Duration,
}

fn engine() -> Result<Arc<Engine>, String> {
    static ENGINE: OnceLock<Mutex<Weak<Engine>>> = OnceLock::new();
    let mut slot = ENGINE
        .get_or_init(|| Mutex::new(Weak::new()))
        .lock()
        .map_err(|e| e.to_string())?;
    if let Some(engine) = slot.upgrade() {
        return Ok(engine);
    }
    let mut config = Config::new();
    config.consume_fuel(true).epoch_interruption(true);
    let engine = Arc::new(Engine::new(&config).map_err(|e| e.to_string())?);
    let weak = Arc::downgrade(&engine);
    std::thread::Builder::new()
        .name("closure-wasm-epoch".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(1));
                let Some(engine) = weak.upgrade() else { break };
                engine.increment_epoch();
            }
        })
        .map_err(|e| e.to_string())?;
    *slot = Arc::downgrade(&engine);
    Ok(engine)
}

fn signature(ty: ExternType, params: &[ValType], results: &[ValType]) -> bool {
    let ExternType::Func(func) = ty else {
        return false;
    };
    func.params()
        .map(|v| v.to_string())
        .eq(params.iter().map(ToString::to_string))
        && func
            .results()
            .map(|v| v.to_string())
            .eq(results.iter().map(ToString::to_string))
}

impl WasmRuntime {
    pub fn load(entry: &[u8]) -> Result<Self, String> {
        let engine = engine()?;
        let module = Module::new(&engine, entry).map_err(|e| e.to_string())?;
        for import in module.imports() {
            if import.module() != "closure"
                || import.name() != "host"
                || !signature(import.ty(), &[ValType::I32, ValType::I32], &[ValType::I64])
            {
                return Err("unsupported import".into());
            }
        }
        if !matches!(module.get_export("memory"), Some(ExternType::Memory(_)))
            || !module
                .get_export("alloc")
                .is_some_and(|ty| signature(ty, &[ValType::I32], &[ValType::I32]))
            || !module
                .get_export("call")
                .is_some_and(|ty| signature(ty, &[ValType::I32, ValType::I32], &[ValType::I64]))
        {
            return Err("invalid guest exports".into());
        }
        Ok(Self { engine, module })
    }
}

fn request(host: &dyn HostApi, value: &Value) -> Result<Value, String> {
    let field = |key: &str| value.get(key).ok_or_else(|| format!("missing {key}"));
    let string = |key: &str| field(key)?.as_str().ok_or_else(|| format!("invalid {key}"));
    match string("op")? {
        "execute" => host.execute(field("action")?.clone()),
        "get" => host.get(string("key")?),
        "set" => {
            host.set(string("key")?, field("value")?.clone())?;
            Ok(Value::Null)
        }
        "delete" => host.delete(string("key")?).map(Value::Bool),
        "list" => host.list(
            string("prefix")?,
            usize::try_from(field("limit")?.as_u64().ok_or("invalid limit")?)
                .map_err(|e| e.to_string())?,
        ),
        _ => Err("unknown operation".into()),
    }
}

fn host_call(mut caller: Caller<'_, State>, ptr: i32, len: i32) -> wasmtime::Result<i64> {
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("missing memory"))?;
    let ptr = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("invalid pointer"))?;
    let len = usize::try_from(len).map_err(|_| wasmtime::Error::msg("invalid length"))?;
    let bytes = memory
        .data(&caller)
        .get(
            ptr..ptr
                .checked_add(len)
                .ok_or_else(|| wasmtime::Error::msg("invalid range"))?,
        )
        .ok_or_else(|| wasmtime::Error::msg("invalid range"))?;
    let input = serde_json::from_slice(bytes).map_err(|e| wasmtime::Error::msg(e.to_string()))?;
    let start = Instant::now();
    let result = request(caller.data().host.as_ref(), &input);
    caller.data_mut().host_time += start.elapsed();
    let response = match result {
        Ok(value) => json!({"ok": value}),
        Err(error) => json!({"err":error}),
    };
    let bytes = serde_json::to_vec(&response).map_err(|e| wasmtime::Error::msg(e.to_string()))?;
    let len = i32::try_from(bytes.len()).map_err(|_| wasmtime::Error::msg("response too large"))?;
    let alloc = caller
        .get_export("alloc")
        .and_then(wasmtime::Extern::into_func)
        .ok_or_else(|| wasmtime::Error::msg("missing alloc"))?
        .typed::<i32, i32>(&caller)?;
    let ptr = alloc.call(&mut caller, len)?;
    let offset = usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("invalid pointer"))?;
    memory.write(&mut caller, offset, &bytes)?;
    Ok(((i64::from(ptr)) << 32) | i64::from(len))
}

impl ScriptRuntime for WasmRuntime {
    fn call(&self, input: Value, host: Arc<dyn HostApi>, budget: Budget) -> Result<Value, String> {
        let run = || -> wasmtime::Result<Value> {
            let limits = StoreLimitsBuilder::new()
                .memory_size(budget.memory_bytes)
                .memories(1)
                .instances(1)
                .tables(1)
                .table_elements(10_000)
                .trap_on_grow_failure(true)
                .build();
            let mut store = Store::new(
                &self.engine,
                State {
                    host,
                    limits,
                    budget,
                    started: Instant::now(),
                    host_time: Duration::ZERO,
                },
            );
            store.limiter(|state| &mut state.limits);
            store.set_fuel(10_000_000_000)?;
            store.set_epoch_deadline(1);
            store.epoch_deadline_callback(|state| {
                let data = state.data();
                if Instant::now() >= data.budget.deadline
                    || data.started.elapsed().saturating_sub(data.host_time)
                        >= Duration::from_millis(data.budget.cpu_ms)
                {
                    Ok(UpdateDeadline::Interrupt)
                } else {
                    Ok(UpdateDeadline::Continue(1))
                }
            });
            let mut linker = Linker::new(&self.engine);
            linker.func_wrap("closure", "host", host_call)?;
            let instance = linker.instantiate(&mut store, &self.module)?;
            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or_else(|| wasmtime::Error::msg("missing memory"))?;
            let alloc = instance.get_typed_func::<i32, i32>(&mut store, "alloc")?;
            let call = instance.get_typed_func::<(i32, i32), i64>(&mut store, "call")?;
            let bytes =
                serde_json::to_vec(&input).map_err(|e| wasmtime::Error::msg(e.to_string()))?;
            let len =
                i32::try_from(bytes.len()).map_err(|_| wasmtime::Error::msg("input too large"))?;
            let ptr = alloc.call(&mut store, len)?;
            memory.write(
                &mut store,
                usize::try_from(ptr).map_err(|_| wasmtime::Error::msg("invalid pointer"))?,
                &bytes,
            )?;
            let result = call.call(&mut store, (ptr, len))?.cast_unsigned();
            if Instant::now() >= budget.deadline
                || store
                    .data()
                    .started
                    .elapsed()
                    .saturating_sub(store.data().host_time)
                    >= Duration::from_millis(budget.cpu_ms)
            {
                return Err(wasmtime::Error::msg("script budget exceeded"));
            }
            let ptr = usize::try_from(result >> 32)?;
            let len = usize::try_from(result & u64::from(u32::MAX))?;
            if len == 0 {
                return Err(wasmtime::Error::msg("empty output"));
            }
            let data = memory
                .data(&store)
                .get(
                    ptr..ptr
                        .checked_add(len)
                        .ok_or_else(|| wasmtime::Error::msg("invalid range"))?,
                )
                .ok_or_else(|| wasmtime::Error::msg("invalid range"))?;
            serde_json::from_slice(data).map_err(|e| wasmtime::Error::msg(e.to_string()))
        };
        run().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BuiltinRuntimes, RuntimeFactory, conformance};
    use closure_core::module::ModuleRuntime;
    use std::fmt::Write;

    fn data(s: &str) -> String {
        s.bytes().fold(String::new(), |mut result, b| {
            write!(result, "\\{b:02x}").unwrap();
            result
        })
    }

    fn guest(body: &str, output: &str, imports: &str) -> String {
        format!(
            r#"(module {imports} (memory (export "memory") 8)
          (data (i32.const 1024) "{}")
          (func (export "alloc") (param i32) (result i32) i32.const 0)
          (func (export "call") (param i32 i32) (result i64) {body} i64.const {}))"#,
            data(output),
            (1024_u64 << 32) | output.len() as u64
        )
    }

    struct Fixtures;
    impl RuntimeFactory for Fixtures {
        fn create(
            &self,
            kind: ModuleRuntime,
            entry: &[u8],
        ) -> Result<Option<Arc<dyn ScriptRuntime>>, String> {
            let name = std::str::from_utf8(entry).map_err(|e| e.to_string())?;
            let wat = match name {
                "echo" => guest("", r#"[{"type":"get_guild","guild_id":"1"}]"#, ""),
                "host_calls" => {
                    let requests = [
                        r#"{"op":"set","key":"key","value":"ok"}"#,
                        r#"{"op":"get","key":"key"}"#,
                        r#"{"op":"execute","action":"ok"}"#,
                    ];
                    let mut extra = String::new();
                    let mut body = String::new();
                    for (i, request) in requests.iter().enumerate() {
                        let ptr = 2048 + i * 256;
                        write!(extra, "(data (i32.const {ptr}) \"{}\")", data(request)).unwrap();
                        write!(
                            body,
                            "i32.const {ptr} i32.const {} call $host drop ",
                            request.len()
                        )
                        .unwrap();
                        if i > 0 {
                            body.push_str("i32.const 0 i64.load i64.const 8008027006923907707 i64.ne if unreachable end ");
                        }
                    }
                    guest(
                        &body,
                        r#"{"action":"ok","stored":"ok"}"#,
                        &format!(
                            "(import \"closure\" \"host\" (func $host (param i32 i32) (result i64))) {extra}"
                        ),
                    )
                }
                "infinite_loop" => guest("(loop $l br $l)", "[]", ""),
                "memory_growth" => guest("(loop $l i32.const 1 memory.grow drop br $l)", "[]", ""),
                "trap" => guest("unreachable", "[]", ""),
                "huge_output" => guest(
                    "",
                    &format!(
                        "[\"{}\"]",
                        "x".repeat(crate::Limits::default().output_bytes + 1)
                    ),
                    "",
                ),
                _ => return Err("unknown fixture".into()),
            };
            BuiltinRuntimes.create(kind, wat.as_bytes())
        }
    }

    #[test]
    fn wasm_conformance() {
        assert_eq!(conformance::run_all_for(&Fixtures, ModuleRuntime::Wasm), []);
    }

    #[test]
    fn rejects_invalid_exports_and_imports() {
        for wat in [
            r#"(module (memory (export "memory") 1) (func (export "call") (param i32 i32) (result i64) i64.const 0))"#,
            r#"(module (memory (export "memory") 1) (func (export "alloc") (result i32) i32.const 0) (func (export "call") (param i32 i32) (result i64) i64.const 0))"#,
            r#"(module (import "other" "host" (func)) (memory (export "memory") 1) (func (export "alloc") (param i32) (result i32) i32.const 0) (func (export "call") (param i32 i32) (result i64) i64.const 0))"#,
        ] {
            assert!(WasmRuntime::load(wat.as_bytes()).is_err());
        }
    }

    struct EmptyHost;
    impl HostApi for EmptyHost {
        fn execute(&self, _: Value) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn get(&self, _: &str) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn set(&self, _: &str, _: Value) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            Ok(false)
        }
        fn list(&self, _: &str, _: usize) -> Result<Value, String> {
            Ok(Value::Null)
        }
    }

    struct SlowHost;
    impl HostApi for SlowHost {
        fn execute(&self, _: Value) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn get(&self, _: &str) -> Result<Value, String> {
            std::thread::sleep(Duration::from_millis(40));
            Ok(Value::Null)
        }
        fn set(&self, _: &str, _: Value) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            Ok(false)
        }
        fn list(&self, _: &str, _: usize) -> Result<Value, String> {
            Ok(Value::Null)
        }
    }

    #[test]
    fn host_wait_excludes_cpu_but_not_deadline() {
        let request = r#"{"op":"get","key":"x"}"#;
        let imports = format!(
            "(import \"closure\" \"host\" (func $host (param i32 i32) (result i64))) (data (i32.const 2048) \"{}\")",
            data(request)
        );
        let wat = guest(
            &format!("i32.const 2048 i32.const {} call $host drop", request.len()),
            "[]",
            &imports,
        );
        let runtime = WasmRuntime::load(wat.as_bytes()).unwrap();
        let budget = Budget {
            cpu_ms: 20,
            memory_bytes: 1024 * 1024,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert_eq!(
            runtime.call(json!({}), Arc::new(SlowHost), budget).unwrap(),
            json!([])
        );
        let budget = Budget {
            deadline: Instant::now() + Duration::from_millis(10),
            ..budget
        };
        assert!(runtime.call(json!({}), Arc::new(SlowHost), budget).is_err());
    }

    #[test]
    fn memory_limit_traps_growth() {
        let wat = r#"(module (memory (export "memory") 1)
            (data (i32.const 1024) "[]")
            (func (export "alloc") (param i32) (result i32) i32.const 0)
            (func (export "call") (param i32 i32) (result i64)
              i32.const 1 memory.grow drop i64.const 4398046511106))"#;
        let runtime = WasmRuntime::load(wat.as_bytes()).unwrap();
        let budget = Budget {
            cpu_ms: 100,
            memory_bytes: 65536,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert!(
            runtime
                .call(json!({}), Arc::new(EmptyHost), budget)
                .is_err()
        );
    }

    #[test]
    fn calls_use_fresh_instances() {
        let wat = r#"(module (memory (export "memory") 1)
            (global $calls (mut i32) (i32.const 0))
            (data (i32.const 1024) "[]")
            (func (export "alloc") (param i32) (result i32) i32.const 0)
            (func (export "call") (param i32 i32) (result i64)
              global.get $calls if unreachable end
              i32.const 1 global.set $calls i64.const 4398046511106))"#;
        let runtime = WasmRuntime::load(wat.as_bytes()).unwrap();
        let budget = Budget {
            cpu_ms: 100,
            memory_bytes: 65536,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        for _ in 0..2 {
            assert_eq!(
                runtime
                    .call(json!({}), Arc::new(EmptyHost), budget)
                    .unwrap(),
                json!([])
            );
        }
    }

    #[test]
    fn invalid_return_range_is_rejected() {
        let wat = r#"(module (memory (export "memory") 1)
            (func (export "alloc") (param i32) (result i32) i32.const 0)
            (func (export "call") (param i32 i32) (result i64) i64.const -4294967295))"#;
        let runtime = WasmRuntime::load(wat.as_bytes()).unwrap();
        let budget = Budget {
            cpu_ms: 100,
            memory_bytes: 1024 * 1024,
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert!(
            runtime
                .call(json!({}), Arc::new(EmptyHost), budget)
                .is_err()
        );
    }
}
