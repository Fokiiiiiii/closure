# Scripting: Lua and WebAssembly modules

Status: the bridge, loader, WebAssembly and Lua runtimes and guest examples are implemented.

## Who installs scripts

closure is self-hosted. **The owner of the host puts scripts on disk**, in `scripts/` next to the
binary (or the directory named by `SCRIPTS_DIR`). There is no upload, no installation from a server
and no per-server script: a server's administrators only choose, in their configuration, which of
the installed modules are on, exactly as for native modules.

The scripts are therefore not hostile, but they are not trusted to be correct either. The sandbox
exists so that a bug, an endless loop or a script copied from somewhere cannot take the bot down or
act beyond what the owner allowed.

## Layout

```
scripts/
  grants.yaml        owner-approved capabilities by module id
  my-feature/
    module.yaml      the manifest
    main.wasm        or main.lua
```

`module.yaml` is the existing `ModuleManifest` as data, plus:

- `runtime: wasm | lua` and `entry: main.wasm`;
- `capabilities`: what the script asks for. The effective set is what it asks for **and** the owner
  grants in `scripts/grants.yaml`, for example `my-feature: [storage]`. A missing file grants nothing. A script starts with
  nothing; a request that is not granted is dropped and logged once at start-up;
- `limits`: optional, tightening only (never above the host's ceilings, see below).

The directory name must equal the module id. Unknown manifest fields, native runtimes, paths outside
the script directory (including symlink escapes), manifests above 256 KiB and entries above 8 MiB are
rejected. Requested limits above the host ceiling are clamped. The generated documentation includes
only `modules()`: installed scripts are local to a particular host.

Commands, subscriptions, settings schema, permission keys and the catalog (`en`/`ja`) are the same
data native modules already declare.

## The boundary

Everything that crosses is JSON of types that already exist and are already `Serialize`:
`Event`, `Action`, `Outcome`, `CommandSpec`.

| Direction | Call | Data |
| --- | --- | --- |
| host → script | `call(event)` | returns a list of actions, including for commands and jobs |
| script → host | `execute(action)` | returns the `Outcome` or an error |
| script → host | `store_get/set/delete/list` | the module's own KV store, same quotas as native |

The current bridge calls the runtime once with the serialized `Event`, using its `type` and `data`
fields, and expects an array of serialized `Action` values. Commands arrive as `command`,
autocomplete as `autocomplete`, and due jobs as `scheduled` (including the complete job payload).
The dispatcher routes commands and autocomplete to their command owner and scheduled jobs to their
owning module, regardless of event subscriptions. Component, modal and context-command events
likewise arrive in their existing `Event` form. An action rejected by the host is logged and later
actions still run. Host KV list returns objects with `key`, `value`, and `revision`.

A script gets no clock, no randomness, no network, no files and no environment beyond what the
event carries. Every `execute` goes through the same `ApiClient` as native
modules: capability check, guild binding, validation. A script cannot do more than a native module
holding the same capabilities.

## Limits (the part that must not be skipped)

`docs/architecture.md` lists a known gap: a native module that blocks a thread is only noticed, not
stopped. For scripts that is not acceptable, so:

- each call runs on a **dedicated blocking pool**, never on the async workers;
- a **budget per call**: WebAssembly fuel plus epoch interruption; Lua an instruction-count hook.
  Default 50 ms of CPU-equivalent and a wall-clock deadline of 250 ms; exceeded, the call is aborted;
- **memory**: 32 MiB for WebAssembly, 16 MiB for Lua, per instance;
- **output**: at most 25 actions and 256 KiB of JSON per call; more is an error;
- **concurrency**: a script runs at most N calls at once (default 4); the rest wait with a short
  queue and are dropped when it is full;
- **circuit breaker**: a script that is aborted or fails five times in a minute is disabled until
  restart, with one error log. The bot never retries a disabled script; `/ping` does not display it.

Every number is a ceiling the host owns; `limits` in the manifest can only lower them.

Known limit of Lua: the instruction hook runs between VM instructions, so a single long call into
a C library function (for example a pathological `string.find` pattern on a large string) is only
noticed when it returns. A call that never returns keeps its worker until the process exits. Memory
is capped, time inside such a call is not. Prefer WebAssembly for code you do not fully trust.

## WebAssembly ABI

The core WebAssembly module exports `memory`, `alloc(len: i32) -> i32` and
`call(ptr: i32, len: i32) -> i64`. The host passes the JSON-encoded `Event` to
`call`. Its result packs the output pointer in the upper 32 bits and the byte
length in the lower 32 bits. The output is a JSON array of `Action`; a zero
length or invalid memory range is an error.

The only permitted import is `closure.host(ptr: i32, len: i32) -> i64`.
The guest sends JSON and the host calls guest `alloc` for the JSON response,
returning the same packed pointer and length. Requests are:

- `{"op":"execute","action":{...}}`
- `{"op":"get","key":"..."}`
- `{"op":"set","key":"...","value":...}`
- `{"op":"delete","key":"..."}`
- `{"op":"list","prefix":"...","limit":10}`

Responses are `{"ok": value}` or `{"err":"message"}`. Calls use a new store
and instance each time; use KV for persistent data. WASI is unavailable.

For example, this WAT module returns an empty action list:

```wat
(module
  (memory (export "memory") 1)
  (data (i32.const 1024) "[]")
  (func (export "alloc") (param i32) (result i32) i32.const 0)
  (func (export "call") (param i32 i32) (result i64)
    i64.const 4398046511106))
```

Each invocation uses fuel, epoch interruption and `StoreLimits`. Epoch checks
exclude time spent in host operations from the CPU budget while still enforcing
the wall deadline. Memory growth failure traps. The `script-wasm` feature is
enabled by default for `closure`; `--no-default-features` omits it. Wasmtime
increases build time and binary size.

## Runtimes

1. **WebAssembly (`wasmtime`)**, available. No WASI, no imports except the host calls above,
   fuel and epoch interruption on, memory and table limits set. Any language that compiles to it
   (Rust, AssemblyScript, JavaScript via a toolchain) can write a module.
2. **Lua (`mlua`, vendored Lua 5.4)**, available for small scripts. The
   standard library is cut down to `string`, `table`, `math`, `utf8` and `coroutine`; `os`, `io`,
   `package`, `debug`, `dofile`, `loadfile` and `load` are absent. An instruction-count hook enforces
   the budget. `loadstring`, `require`, `collectgarbage`, `string.dump`, `math.random` and
   `math.randomseed` are also absent.

Lua source must be text, and is syntax checked when loaded. Each event gets a fresh Lua state.
The chunk defines `on_event(event)`, which returns an array of actions; `nil` and `{}` mean no
actions. JSON arrays carry an array metatable and JSON null is `closure.null`; use
`closure.array({})` for a newly constructed empty array. `closure.execute(action)`,
`closure.store_get(key)`, `closure.store_set(key, value)`, `closure.store_delete(key)` and
`closure.store_list(prefix, limit)` raise Lua errors on host errors. An upper-limit failure still
fails the invocation even if caught with `pcall`.

```lua
function on_event(event)
  return {}
end
```

The instruction hook checks every 1,000 instructions, including coroutines. It checks the wall
deadline and elapsed CPU-equivalent time excluding time in host calls. Lua allocations have a
16 MiB ceiling, or the manifest's lower limit. The `closure` binary enables `script-lua` by default;
use `--no-default-features` to omit it. The runtime factory is available to the application wiring.

Both runtimes implement one small internal trait (`ScriptRuntime`: load, call, drop), so the bridge,
the limits, the circuit breaker and the tests are written once. A second runtime is a thin layer.

New dependencies (`wasmtime`, `mlua`) are large; they are added only with the owner's approval, and
behind a Cargo feature so a build that does not want them does not pay for them.

## Loading

- Scripts are read once at start-up in directory-name order. Each invalid script is skipped and
  logged without preventing other scripts from loading. Ungranted capabilities are dropped with
  one warning per module. A build without the relevant runtime reports the script as unavailable.
- A manifest is validated like a native one: unique names, command and option limits, schema.
- Reloading while running is not in the first version. It needs the unload of a module's commands,
  jobs and subscriptions to be exact; it comes after the bridge is proven.

## Tests

- A **fake runtime** (a Rust closure per call) drives the bridge: capability narrowing, action
  validation, limits, the circuit breaker and the queue are tested without any engine.
- A **conformance suite** of tiny scripts is run against each real runtime: an echo, an endless loop
  (must be aborted within the deadline), a memory hog, host execute and KV calls, a huge output and
  a trap. Adapters provide case fixtures to `closure_script::conformance::run_all`; a runtime must
  stop execution within its budget and reject excess memory; the host rejects excess output. The fake runtime demonstrates
  the missing loop and memory enforcement; WebAssembly and Lua fixtures exercise the real runtimes.
  `run_all_for` selects Lua or WebAssembly. A runtime that never returns leaves its test worker
  running, so run untrusted adapters in a disposable process.

## Not in the first version

Uploads and per-server scripts, WASI or any file or network access, reloading while running,
calls between modules, scripts writing their own commands at run time, and a package format.

## Order of work

| Step | What |
| --- | --- |
| S1 | `ScriptModule`, bridge, limits, circuit breaker and fake runtime: complete |
| S2 | WebAssembly runtime and conformance suite: complete |
| S3 | Lua runtime against the same suite: complete |
| S4 | guest examples and [writing modules](writing-modules.md): complete |
