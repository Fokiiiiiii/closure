use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::{
    HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Value as LuaValue, VmState,
    chunk::ChunkMode,
};
use serde_json::Value;

use crate::runtime::{Budget, HostApi, ScriptRuntime};

/// A validated Lua source, evaluated in a fresh state for every invocation.
#[derive(Debug)]
pub struct LuaRuntime(Vec<u8>);

impl LuaRuntime {
    pub fn new(entry: &[u8]) -> Result<Self, String> {
        let lua = Lua::new();
        lua.load(entry)
            .set_mode(ChunkMode::Text)
            .into_function()
            .map_err(|e| e.to_string())?;
        Ok(Self(entry.to_vec()))
    }
}

#[cfg(test)]
mod tests;

impl ScriptRuntime for LuaRuntime {
    fn call(&self, input: Value, host: &dyn HostApi, budget: Budget) -> Result<Value, String> {
        self.invoke(&input, host, budget).map_err(|e| e.to_string())
    }
}

impl LuaRuntime {
    fn invoke(&self, input: &Value, host: &dyn HostApi, budget: Budget) -> mlua::Result<Value> {
        let (lua, waited, interrupted, started) = sandbox(budget)?;
        let globals = lua.globals();
        lua.scope(|scope| {
            let api = lua.create_table()?;
            macro_rules! host_call {
                ($name:literal, $lua:ident, $waited:ident, $args:pat, $ty:ty, $body:expr) => {{
                    let $waited = Rc::clone(&waited);
                    api.set($name, scope.create_function(move |$lua, $args: $ty| $body)?)?;
                }};
            }
            host_call!("execute", lua, waited, action, LuaValue, {
                let action = lua.from_value(action)?;
                lua.to_value(&timed(&waited, || {
                    host.execute(action).map_err(mlua::Error::runtime)
                })?)
            });
            host_call!("store_get", lua, waited, key, String, {
                lua.to_value(&timed(&waited, || {
                    host.get(&key).map_err(mlua::Error::runtime)
                })?)
            });
            host_call!(
                "store_set",
                lua,
                waited,
                (key, value),
                (String, LuaValue),
                {
                    let value = lua.from_value(value)?;
                    timed(&waited, || {
                        host.set(&key, value).map_err(mlua::Error::runtime)
                    })
                }
            );
            host_call!("store_delete", _lua, waited, key, String, {
                timed(&waited, || host.delete(&key).map_err(mlua::Error::runtime))
            });
            host_call!(
                "store_list",
                lua,
                waited,
                (prefix, limit),
                (String, usize),
                {
                    lua.to_value(&timed(&waited, || {
                        host.list(&prefix, limit).map_err(mlua::Error::runtime)
                    })?)
                }
            );
            api.set("null", lua.null())?;
            api.set(
                "array",
                lua.create_function(|lua, table: mlua::Table| {
                    table.set_metatable(Some(lua.array_metatable()))?;
                    Ok(table)
                })?,
            )?;
            globals.set("closure", api)?;
            lua.load(&self.0).set_mode(ChunkMode::Text).exec()?;
            let result: LuaValue = globals
                .get::<mlua::Function>("on_event")?
                .call(lua.to_value(input)?)?;
            if interrupted.get() || Instant::now() >= budget.deadline {
                return Err(mlua::Error::runtime("script budget exceeded"));
            }
            if let Some(error) = host.limit_failure() {
                return Err(mlua::Error::runtime(error));
            }
            let value = match result {
                LuaValue::Nil => Ok(Value::Array(vec![])),
                LuaValue::Table(ref table) if table.is_empty() && table.metatable().is_none() => {
                    Ok(Value::Array(vec![]))
                }
                _ => lua.from_value(result),
            }?;
            let now = Instant::now();
            if now >= budget.deadline
                || now.duration_since(started).saturating_sub(waited.get())
                    >= Duration::from_millis(budget.cpu_ms)
            {
                return Err(mlua::Error::runtime("script budget exceeded"));
            }
            Ok(value)
        })
    }
}

fn timed<T>(waited: &Cell<Duration>, f: impl FnOnce() -> mlua::Result<T>) -> mlua::Result<T> {
    let start = Instant::now();
    let result = f();
    waited.set(waited.get() + start.elapsed());
    result
}

type Sandbox = (Lua, Rc<Cell<Duration>>, Rc<Cell<bool>>, Instant);

fn sandbox(budget: Budget) -> mlua::Result<Sandbox> {
    let started = Instant::now();
    let waited = Rc::new(Cell::new(Duration::ZERO));
    let interrupted = Rc::new(Cell::new(false));
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(budget.memory_bytes.min(16 * 1024 * 1024))?;
    let globals = lua.globals();
    for name in [
        "dofile",
        "loadfile",
        "load",
        "loadstring",
        "require",
        "collectgarbage",
    ] {
        globals.set(name, LuaValue::Nil)?;
    }
    globals
        .get::<mlua::Table>("string")?
        .set("dump", LuaValue::Nil)?;
    let math = globals.get::<mlua::Table>("math")?;
    math.set("random", LuaValue::Nil)?;
    math.set("randomseed", LuaValue::Nil)?;
    let hook_waited = Rc::clone(&waited);
    let hook_interrupted = Rc::clone(&interrupted);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(1000),
        move |_, _| {
            let now = Instant::now();
            if now >= budget.deadline
                || now
                    .duration_since(started)
                    .saturating_sub(hook_waited.get())
                    >= Duration::from_millis(budget.cpu_ms)
            {
                hook_interrupted.set(true);
                return Err(mlua::Error::runtime("script budget exceeded"));
            }
            Ok(VmState::Continue)
        },
    )?;
    {
        let original = globals.get::<mlua::Function>("pcall")?;
        let interrupted = Rc::clone(&interrupted);
        globals.set(
            "pcall",
            lua.create_function(move |_, args: mlua::MultiValue| {
                let result = original.call::<mlua::MultiValue>(args)?;
                if interrupted.get() {
                    return Err(mlua::Error::runtime("script budget exceeded"));
                }
                Ok(result)
            })?,
        )?;
    }
    lua.load("local p, pack, unpack = pcall, table.pack, table.unpack; xpcall = function(f, handler, ...) local r = pack(p(f, ...)); if r[1] then return unpack(r, 1, r.n) end; local h = pack(p(handler, r[2])); if not h[1] then return false, 'error in error handling' end; return false, h[2] end").set_mode(ChunkMode::Text).exec()?;
    let coroutine = globals.get::<mlua::Table>("coroutine")?;
    coroutine.set(
        "create",
        lua.create_function(|lua, function: mlua::Function| lua.create_thread(function))?,
    )?;
    lua.load("coroutine.wrap = function(f) local co = coroutine.create(f); return function(...) local r = table.pack(coroutine.resume(co, ...)); if not r[1] then error(r[2], 2) end; return table.unpack(r, 2, r.n) end end").set_mode(ChunkMode::Text).exec()?;
    Ok((lua, waited, interrupted, started))
}
