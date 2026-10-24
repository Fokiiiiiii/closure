# Architecture

closure is a Cargo workspace. The dependency direction is one-way: the binary depends on
everything, and everything depends downwards on `closure-core`, which depends on nothing of ours.

```text
closure (binary)
 ├─ closure-api ───────┐
 ├─ closure-discord ───┼─► closure-config ─► closure-core
 ├─ closure-db ────────┘        closure-i18n ─► closure-core
 └─ closure-mod-* (modules) ──────────────────► closure-core
```

| Crate | Responsibility |
| --- | --- |
| `closure-core` | The contract: `Module`, manifests, the stable `Event` model, `Capability`, the serialisable `Action`/`Outcome` boundary, `ApiClient`, `ModuleRegistry`, locales. Depends on `twilight-model` for ids and payloads only. |
| `closure-i18n` | Fluent-backed `Translator` with English and Japanese catalogs and a fallback chain. |
| `closure-config` | The configuration document, YAML in and out, validation, JSON Schema, and `SettingsStore` (the per-guild settings the bot acts on). |
| `closure-db` | PostgreSQL: configuration with revisions and history, change notifications, audit trail. |
| `closure-discord` | Twilight: REST (`TwilightApi`), gateway shards, event conversion, the event `Dispatcher`, slash command publishing. |
| `closure-api` | The HTTP API (Axum). |
| `closure-mod-diagnostics` | `/ping` and `/about`; the reference module. |
| `closure` | The composition root: reads settings, wires everything, shuts it down in order. |

## From a Discord event to a Discord action

```text
gateway shard ─► cache.update ─► convert ─► guild hooks ─► Dispatcher ─► lane ─► Module::on_event
                                                                                        │
                       Discord ◄─ TwilightApi ◄─ ApiClient (capability + validation) ◄──┘
```

1. A shard receives a payload. The cache is updated, then the payload is converted to the stable
   `Event` model. Direct messages and guild outages are dropped here; every later stage can rely on
   an event belonging to a guild.
2. Guild hooks (recording the guild, its Discord language, restoring its configuration on
   rejoin) run in order on a background task right after the event is dispatched. A slow database
   therefore never stops the shard from being read, which would get the connection cut; the cost
   is that a module may see a guild arrive a moment before the hook has finished.
3. The `Dispatcher` picks the guild's *lane* from the channel, user or role the event concerns.
   Events in one lane run in order; lanes run in parallel. Slash commands use lanes of their own.
   The guild has one budget of events waiting or running (commands have another): when it is used
   up the event is dropped and counted, never buffered without limit.
4. Each module call runs in its own task with a timeout. A panic or a hang costs that call only.
   After repeated failures a module is paused for that guild and then given one trial call (circuit
   breaker). Errors that mean "Discord refused" do not count against the module.
5. The module asks for Discord side effects through `ApiClient`. It rejects an action the module
   has no capability for, validates it (lengths, ranges, required audit-log reason), and only then
   hands it to `TwilightApi`.
6. `TwilightApi` consults the cache for the role hierarchy. What is certain (the bot targeting
   itself, the server owner) is refused up front; a rank problem is only a *suspicion*, because
   the cache can be stale. The request is sent anyway, and if Discord answers 403 the suspicion
   turns a vague `Missing Permissions` into a precise `HierarchyViolation`. It then normalises
   failures into `DiscordError`, retries *reads* on transient failures (never writes), and
   records metrics.

A module never receives an HTTP client, a token or a database handle. Its `ApiClient` is bound to
the guild of the event: an action that names another guild is refused, whatever capabilities the
module holds. Actions that identify only a channel or a message name no guild and cannot be checked
that way; that is acceptable for trusted native modules, but must be closed (by resolving the
channel's guild) before untrusted scripted modules are allowed to run.

## Configuration

The database stores, per guild, one sparse document (`GuildOverrides`) with a revision and a
history. At start-up every document is compiled — merged over the preset and module defaults,
checked against each module's JSON Schema, capabilities capped at what the module declares — and
installed into the `SettingsStore`. Edits through the API are validated *before* they are stored.
Other processes learn of an edit through PostgreSQL `LISTEN/NOTIFY`; because notifications are not
durable, everything is reloaded whenever the connection may have dropped some. The store only
accepts an installation whose revision is at least as new as the one it holds, so concurrent
writers cannot let a stale read win. A stored document that no longer validates is skipped with a
warning and the guild keeps its previous settings.

See [configuration.md](configuration.md) for the document itself.

## Languages

A `Locale` is a language the bot can speak. Messages live in Fluent catalogs that each module
declares in its manifest (namespace = module id), plus a shared `core` catalog for errors. Lookup
falls back from the requested language to English, then to the shared catalog, then to the key
itself; a catalog mistake can degrade a message but never break an action. Which language is used:

- command replies: the language of the person who ran the command, else the guild's;
- everything else: the guild's `language` setting, else the community language the server chose in
  Discord (`language: auto`), else English.

Slash command names and descriptions are published in every language that has them in the catalog
(see `crates/closure-discord/src/commands.rs` for the naming convention). Adding a language means a
new `Locale` variant plus a catalog per module; tests check that all catalogs define the same
messages.

## Extending: Lua and WebAssembly

Only native modules exist today, but the contract was shaped for more:

- a `ModuleManifest` is plain data (it can be read from a file);
- `Event` and `Action`/`Outcome` are serialisable, so a script host can receive events and return
  actions without a Rust API;
- capabilities are the sandbox boundary: a module whose runtime is not `Native` starts with *no*
  capabilities and receives only what a server administrator grants, never more than it declares.

A script host would implement `Module` by serialising the event into the script and turning its
returned actions into `ApiClient::execute` calls. Nothing else in the system would change.

## Decisions worth knowing

- **Queries are plain SQL, not compile-time checked.** Contributors can build without a running
  database; every query is exercised against a real PostgreSQL in the tests.
- **Gateway intents are the union over all registered modules.** Intents belong to the connection,
  not to a guild, so they cannot follow per-guild enablement.
- **Slash commands are published for every registered module.** A disabled module's command still
  appears; invoking it gets a private "that feature is turned off" answer.
- **Command specs are data**, not Twilight derive macros, so scripted modules can declare commands
  the same way.

## Known gaps

- `task.abort()` stops a timed-out module only at an `await`; a module that blocks a thread or
  spawns blocking work keeps running. Modules must not block.
- The HTTP API has no connection cap or header-read timeout of its own, and `/readyz` queries the
  database on every call. Put it behind a reverse proxy if it is reachable by anyone untrusted.
- Command events go only to the module that owns the command, not to other subscribers.
- Subcommand *groups* (two levels of nesting) are not supported; one level is.
- A stored document that fails to parse makes `GET /v1/guilds/{id}/config` return 500 for that
  guild, and a replacement needs the revision from the database.
- The HTTP API has no rate limit on failed logins; keep it on loopback or behind a proxy.
- One process per bot; shard ranges are not configurable.
