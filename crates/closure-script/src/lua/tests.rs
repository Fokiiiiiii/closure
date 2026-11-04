use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use closure_core::module::ModuleRuntime;
use serde_json::{Value, json};

use super::*;
use crate::{BuiltinRuntimes, RuntimeFactory, conformance};

#[derive(Default)]
struct Host(Mutex<Value>);
impl HostApi for Host {
    fn execute(&self, value: Value) -> Result<Value, String> {
        Ok(value)
    }
    fn get(&self, _: &str) -> Result<Value, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn set(&self, _: &str, value: Value) -> Result<(), String> {
        *self.0.lock().unwrap() = value;
        Ok(())
    }
    fn delete(&self, _: &str) -> Result<bool, String> {
        Ok(true)
    }
    fn list(&self, _: &str, _: usize) -> Result<Value, String> {
        Ok(json!([]))
    }
}

fn budget() -> Budget {
    Budget {
        cpu_ms: 20,
        memory_bytes: 1024 * 1024,
        output_bytes: crate::Limits::default().output_bytes,
        deadline: Instant::now() + Duration::from_millis(100),
    }
}

#[test]
fn finalizers_are_rejected_but_regular_metatables_work() {
    let source = b"function on_event() local t=setmetatable({}, {__index={x=7}}); assert(t.x==7); assert(not pcall(setmetatable, {}, {__gc=function() end})); assert(getmetatable(closure.array({})) == false); local mt=getmetatable(''); mt.__gc=function() end; assert(not pcall(setmetatable, {}, mt)); return {} end";
    let start = Instant::now();
    assert_eq!(
        LuaRuntime::new(source)
            .unwrap()
            .call(json!({}), Arc::new(Host::default()), budget())
            .unwrap(),
        json!([])
    );
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[test]
fn expanding_json_is_rejected_at_all_lua_boundaries() {
    for body in [
        "return {t}",
        "closure.execute(t); return {}",
        "closure.store_set('x', t); return {}",
    ] {
        let source =
            format!("function on_event() local t='x'; for i=1,12 do t={{t,t}} end; {body} end");
        let error = LuaRuntime::new(source.as_bytes())
            .unwrap()
            .call(
                json!({}),
                Arc::new(Host::default()),
                Budget {
                    output_bytes: 128,
                    ..budget()
                },
            )
            .unwrap_err();
        assert!(error.contains("output exceeds the limit"), "{error}");
    }
}

#[test]
fn json_conversion_stops_at_the_configured_limit() {
    let runtime = LuaRuntime::new(
        b"function on_event() local t='x'; for i=1,12 do t={t,t} end; return {t} end",
    )
    .unwrap();
    let budget = Budget {
        output_bytes: 128,
        ..budget()
    };
    assert!(
        runtime
            .call(json!({}), Arc::new(Host::default()), budget)
            .unwrap_err()
            .contains("output exceeds the limit")
    );
    let runtime = LuaRuntime::new(b"function on_event() return { {a='ok'} } end").unwrap();
    assert_eq!(
        runtime
            .call(json!({}), Arc::new(Host::default()), budget)
            .unwrap(),
        json!([{"a":"ok"}])
    );
}

#[test]
fn caught_host_conversion_limit_still_fails_invocation() {
    for call in ["closure.execute(t)", "closure.store_set('x', t)"] {
        let source = format!(
            "function on_event() local t=string.rep('x', 256); pcall(function() {call} end); return {{}} end"
        );
        let result = LuaRuntime::new(source.as_bytes()).unwrap().call(
            json!({}),
            Arc::new(Host::default()),
            Budget {
                output_bytes: 128,
                ..budget()
            },
        );
        assert!(result.unwrap_err().contains("output exceeds the limit"));
    }
}

#[test]
fn sparse_arrays_match_existing_json_conversion() {
    let (lua, _, _, _) = sandbox(budget()).unwrap();
    let value: LuaValue = lua
        .load("local t={'a','b','c'}; t[2]=nil; return t")
        .eval()
        .unwrap();
    let expected: Value = lua.from_value(value.clone()).unwrap();
    assert_eq!(bounded_json(&lua, value, 1024).unwrap(), expected);
}

struct Fixtures;
impl RuntimeFactory for Fixtures {
    fn create(
        &self,
        kind: ModuleRuntime,
        entry: &[u8],
    ) -> Result<Option<Arc<dyn ScriptRuntime>>, String> {
        let source = match entry {
            b"echo" => {
                "function on_event(e) return {{type='get_guild', guild_id=e.data.guild_id}} end"
            }
            b"host_calls" => {
                "function on_event(e) closure.store_set('key', 'ok'); return {action=closure.execute('ok'), stored=closure.store_get('key')} end"
            }
            b"infinite_loop" => "function on_event(e) while true do end end",
            b"memory_growth" => {
                "function on_event(e) local t={} while true do t[#t+1]=string.rep('x',10000) end end"
            }
            b"trap" => "function on_event(e) error('trap') end",
            b"huge_output" => "function on_event(e) return {string.rep('x',300000)} end",
            _ => return Err("unknown fixture".into()),
        };
        BuiltinRuntimes.create(kind, source.as_bytes())
    }
}

#[test]
fn lua_conformance() {
    assert_eq!(conformance::run_all_for(&Fixtures, ModuleRuntime::Lua), []);
}

#[test]
fn lua_sandbox_and_json_roundtrip() {
    let source = r"function on_event(e)
        assert(os == nil and io == nil and package == nil and debug == nil)
        assert(dofile == nil and loadfile == nil and load == nil and loadstring == nil and require == nil and collectgarbage == nil)
        assert(string.dump == nil and math.random == nil and math.randomseed == nil)
        assert(string and table and math and utf8 and coroutine)
        local ok, value = xpcall(function() error('failure') end, function() return 'handled' end)
        assert(not ok and value == 'handled')
        local good, one, two = xpcall(function(a) return a, 2 end, function() end, 1)
        assert(good and one == 1 and two == 2)
        closure.store_set('x', e)
        local value = closure.store_get('x')
        assert(closure.store_delete('x'))
        assert(#closure.store_list('', 10) == 0)
        return {value}
    end";
    let input =
        json!({"empty_array": [], "empty_object": {}, "null": null, "nested": [null, [], {}]});
    let runtime = LuaRuntime::new(source.as_bytes()).unwrap();
    assert_eq!(
        runtime
            .call(input.clone(), Arc::new(Host::default()), budget())
            .unwrap(),
        json!([input])
    );
    for src in [
        "function on_event(e) end",
        "function on_event(e) return {} end",
    ] {
        assert_eq!(
            LuaRuntime::new(src.as_bytes())
                .unwrap()
                .call(json!({}), Arc::new(Host::default()), budget())
                .unwrap(),
            json!([])
        );
    }
    assert!(LuaRuntime::new(b"function (").is_err());
    assert!(LuaRuntime::new(b"\x1bLua\x54").is_err());
}

#[test]
fn lua_interrupts_caught_and_coroutine_loops() {
    for body in [
        "while true do end",
        "while true do pcall(function() while true do end end) end",
        "while true do xpcall(function() while true do end end, function() end) end",
        "xpcall(function() error('x') end, function() while true do end end)",
        "local c=coroutine.create(function() while true do end end); while true do coroutine.resume(c) end",
        "local f=coroutine.wrap(function() while true do end end); while true do pcall(f) end",
    ] {
        let source = format!("function on_event(e) {body} end");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = LuaRuntime::new(source.as_bytes()).unwrap().call(
                json!({}),
                Arc::new(Host::default()),
                budget(),
            );
            let _ = tx.send(result);
        });
        assert!(
            rx.recv_timeout(Duration::from_secs(1))
                .unwrap_or_else(|_| panic!("runtime did not stop: {body}"))
                .is_err()
        );
    }
}

#[test]
fn lua_state_is_fresh_and_host_failures_propagate() {
    struct FailingHost;
    impl HostApi for FailingHost {
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
        fn limit_failure(&self) -> Option<String> {
            Some("limit failure".into())
        }
    }
    let runtime =
        LuaRuntime::new(b"n = (n or 0) + 1; function on_event(e) return {n} end").unwrap();
    for _ in 0..2 {
        assert_eq!(
            runtime
                .call(json!({}), Arc::new(Host::default()), budget())
                .unwrap(),
            json!([1])
        );
    }
    let runtime =
        LuaRuntime::new(b"function on_event(e) pcall(closure.store_get, 'x'); return {} end")
            .unwrap();
    assert!(
        runtime
            .call(json!({}), Arc::new(FailingHost), budget())
            .unwrap_err()
            .contains("limit failure")
    );
    let runtime = LuaRuntime::new(b"function on_event(e) closure.store_get('x') end").unwrap();
    assert!(
        runtime
            .call(json!({}), Arc::new(FailingHost), budget())
            .unwrap_err()
            .contains("denied")
    );
}

#[test]
fn lua_memory_limit_blocks_single_allocation() {
    let runtime =
        LuaRuntime::new(b"function on_event(e) return {string.rep('x', 2000000)} end").unwrap();
    assert!(
        runtime
            .call(json!({}), Arc::new(Host::default()), budget())
            .is_err()
    );
}

#[test]
fn lua_host_wait_is_excluded_from_cpu_budget() {
    struct SlowHost;
    impl HostApi for SlowHost {
        fn execute(&self, _: Value) -> Result<Value, String> {
            unreachable!()
        }
        fn get(&self, _: &str) -> Result<Value, String> {
            std::thread::sleep(Duration::from_millis(40));
            Ok(Value::Null)
        }
        fn set(&self, _: &str, _: Value) -> Result<(), String> {
            unreachable!()
        }
        fn delete(&self, _: &str) -> Result<bool, String> {
            unreachable!()
        }
        fn list(&self, _: &str, _: usize) -> Result<Value, String> {
            unreachable!()
        }
    }
    let runtime = LuaRuntime::new(b"function on_event(e) closure.store_get('x'); local n=0; for i=1,10000 do n=n+i end; return {} end").unwrap();
    let budget = Budget {
        cpu_ms: 20,
        memory_bytes: 1024 * 1024,
        output_bytes: crate::Limits::default().output_bytes,
        deadline: Instant::now() + Duration::from_millis(500),
    };
    assert_eq!(
        runtime.call(json!({}), Arc::new(SlowHost), budget).unwrap(),
        json!([])
    );
}
