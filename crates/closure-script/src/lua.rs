use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
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
    fn call(&self, input: Value, host: Arc<dyn HostApi>, budget: Budget) -> Result<Value, String> {
        self.invoke(&input, host.as_ref(), budget)
            .map_err(|e| e.to_string())
    }
}

impl LuaRuntime {
    fn invoke(&self, input: &Value, host: &dyn HostApi, budget: Budget) -> mlua::Result<Value> {
        let (lua, waited, interrupted, started) = sandbox(budget)?;
        let globals = lua.globals();
        let conversion_limit = Rc::new(Cell::new(false));
        lua.scope(|scope| {
            let api = lua.create_table()?;
            macro_rules! host_call {
                ($name:literal, $lua:ident, $waited:ident, $args:pat, $ty:ty, $body:expr) => {{
                    let $waited = Rc::clone(&waited);
                    api.set($name, scope.create_function(move |$lua, $args: $ty| $body)?)?;
                }};
            }
            let execute_limit = Rc::clone(&conversion_limit);
            host_call!("execute", lua, waited, action, LuaValue, {
                let action = bounded_host_json(lua, action, budget.output_bytes, &execute_limit)?;
                lua.to_value(&timed(&waited, || {
                    host.execute(action).map_err(mlua::Error::runtime)
                })?)
            });
            host_call!("store_get", lua, waited, key, String, {
                lua.to_value(&timed(&waited, || {
                    host.get(&key).map_err(mlua::Error::runtime)
                })?)
            });
            let store_limit = Rc::clone(&conversion_limit);
            host_call!(
                "store_set",
                lua,
                waited,
                (key, value),
                (String, LuaValue),
                {
                    let value = bounded_host_json(lua, value, budget.output_bytes, &store_limit)?;
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
                    let metatable = lua.array_metatable();
                    if !matches!(metatable.raw_get::<LuaValue>("__gc")?, LuaValue::Nil) {
                        return Err(mlua::Error::runtime("finalizers are not permitted"));
                    }
                    table.set_metatable(Some(metatable))?;
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
            if conversion_limit.get() {
                return Err(mlua::Error::runtime("script output exceeds the limit"));
            }
            if let Some(error) = host.limit_failure() {
                return Err(mlua::Error::runtime(error));
            }
            let value = match result {
                LuaValue::Nil => Ok(Value::Array(vec![])),
                LuaValue::Table(ref table) if table.is_empty() && table.metatable().is_none() => {
                    Ok(Value::Array(vec![]))
                }
                _ => bounded_json(&lua, result, budget.output_bytes),
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

fn bounded_host_json(
    lua: &Lua,
    value: LuaValue,
    limit: usize,
    exceeded: &Cell<bool>,
) -> mlua::Result<Value> {
    bounded_json(lua, value, limit).inspect_err(|error| {
        if matches!(error, mlua::Error::RuntimeError(message) if message == "script output exceeds the limit") {
            exceeded.set(true);
        }
    })
}

fn bounded_json(lua: &Lua, value: LuaValue, limit: usize) -> mlua::Result<Value> {
    fn convert(
        lua: &Lua,
        value: LuaValue,
        remaining: &mut usize,
        nodes: &mut usize,
        depth: usize,
    ) -> mlua::Result<Value> {
        if depth > 32 {
            return Err(mlua::Error::runtime("script output exceeds the limit"));
        }
        charge_remaining(nodes, 1)?;
        let mut charge = |size: usize| -> mlua::Result<()> {
            *remaining = remaining
                .checked_sub(size)
                .ok_or_else(|| mlua::Error::runtime("script output exceeds the limit"))?;
            Ok(())
        };
        match value {
            LuaValue::Table(table) => {
                charge(2)?;
                if table.raw_len() > 0
                    || table
                        .metatable()
                        .is_some_and(|mt| mt == lua.array_metatable())
                {
                    let mut values = Vec::new();
                    for index in 1..=table.raw_len() {
                        if !values.is_empty() {
                            charge_remaining(remaining, 1)?;
                        }
                        values.push(convert(
                            lua,
                            table.raw_get(index)?,
                            remaining,
                            nodes,
                            depth + 1,
                        )?);
                    }
                    Ok(Value::Array(values))
                } else {
                    let mut values = serde_json::Map::new();
                    for pair in table.pairs::<LuaValue, LuaValue>() {
                        let (key, item) = pair?;
                        let LuaValue::String(key) = key else {
                            return Err(mlua::Error::runtime("invalid JSON object key"));
                        };
                        if key.as_bytes().len() > *remaining {
                            return Err(mlua::Error::runtime("script output exceeds the limit"));
                        }
                        let key = key.to_str()?.to_owned();
                        let size =
                            crate::runtime::json_size(&Value::String(key.clone()), *remaining)
                                .map_err(mlua::Error::runtime)?;
                        charge_remaining(remaining, size + 1 + usize::from(!values.is_empty()))?;
                        values.insert(key, convert(lua, item, remaining, nodes, depth + 1)?);
                    }
                    Ok(Value::Object(values))
                }
            }
            other => {
                if let LuaValue::String(ref s) = other
                    && s.as_bytes().len() > *remaining
                {
                    return Err(mlua::Error::runtime("script output exceeds the limit"));
                }
                let value: Value = lua.from_value(other)?;
                let size =
                    crate::runtime::json_size(&value, *remaining).map_err(mlua::Error::runtime)?;
                charge_remaining(remaining, size)?;
                Ok(value)
            }
        }
    }
    fn charge_remaining(remaining: &mut usize, size: usize) -> mlua::Result<()> {
        *remaining = remaining
            .checked_sub(size)
            .ok_or_else(|| mlua::Error::runtime("script output exceeds the limit"))?;
        Ok(())
    }
    convert(lua, value, &mut limit.clone(), &mut limit.clone(), 0)
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
    lua.array_metatable().raw_set("__metatable", false)?;
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
    lua.load("local original, raw = setmetatable, rawget; setmetatable = function(t, mt) if mt ~= nil and raw(mt, '__gc') ~= nil then error('finalizers are not permitted', 2) end return original(t, mt) end").set_mode(ChunkMode::Text).exec()?;
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
