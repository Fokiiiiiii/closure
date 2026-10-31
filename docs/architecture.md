# Architecture

closure is a Cargo workspace. The dependency direction is one-way: the binary depends on
everything, and everything depends downwards on `closure-core`, which depends on nothing of ours.

What is built and what is planned next: [roadmap](roadmap.md).

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
| `closure-db` | PostgreSQL: configuration with revisions and history, change notifications, audit trail, the data modules keep, and the jobs they schedule. |
| `closure-discord` | Twilight: REST (`TwilightApi`), gateway shards, event conversion, the event `Dispatcher`, slash command publishing. |
| `closure-api` | The HTTP API (Axum). |
| `closure-sdk`, `closure-sdk-macros` | What a module author uses: declare commands (`#[derive(CommandArgs)]`), permissions, buttons and forms, jobs; the SDK checks, parses, answers and tests (`testing::Harness`). |
| `closure-mod-diagnostics` | `/ping`; the smallest module. |
| `closure-mod-moderation` | `/warn`, `/note`, `/warnings` and `/case`; the reference module written with the SDK only: permissions, a cooldown, cases, the log, direct messages, two languages. |
| `closure-mod-automod` | [`automod`](automod.md): rules that check every message and act on the first one that hits; off until a server turns it on. Its frame only: the detailed rules come later. |
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
   rejoin) run on a background task right after the event is dispatched. Pending hooks are kept
   per guild and only the newest one counts (a departure replaces a queued arrival), so the queue
   is bounded by the number of guilds and pushing never waits. A slow database therefore never
   stops the shard from being read, which would get the connection cut; the cost is that a
   module may see a guild arrive a moment before the hook has finished. Because the gateway only
   reports changes while connected, start-up first reads the bot's guild list from Discord and
   records guilds joined or left during downtime. The hook task ends once the last shard has
   finished, after running what is still queued.
3. The `Dispatcher` picks the guild's *lane* from the channel, user or role the event concerns.
   Events in one lane run in order; lanes run in parallel. Slash commands use lanes of their own.
   The guild has one budget of events waiting or running (commands have another): when it is used
   up the event is dropped and counted, never buffered without limit.
4. Each module call runs in its own task with a timeout. A panic or a hang costs that call only.
   After repeated failures a module is paused for that guild and then given one trial call (circuit
   breaker). Errors that mean "Discord refused" do not count against the module, and the result
   of a call admitted before the breaker opened is ignored, so it cannot decide the trial.
   On shutdown, module calls that overrun the grace period are aborted and awaited before any
   module is unloaded. A call that overruns its timeout is aborted; if it does not stop within a
   second it blocks a thread (aborting only works at an `await`). The module is then *wedged*:
   it gets no new calls, in any guild, until the running ones have returned, so it cannot take
   one worker thread after another. This is logged once, counted as
   `closure_handler_total{result="wedged"}` and shown by `closure_module_wedged_calls`.
5. The module asks for Discord side effects through `ApiClient`. It rejects an action the module
   has no capability for, validates it (lengths, ranges, required audit-log reason), and only then
   hands it to `TwilightApi`.
6. `TwilightApi` consults the cache for the role hierarchy. What is certain (the bot targeting
   itself, the server owner) is refused up front; a rank problem is only a *suspicion*, because
   the cache can be stale. The request is sent anyway, and if Discord answers 403 the suspicion
   turns a vague `Missing Permissions` into a precise `HierarchyViolation`. It then normalises
   failures into `DiscordError`, retries *reads* on transient failures (never writes), and
   records metrics.

A module never receives an HTTP client, a token or a database handle. What it keeps (entries and
counters, with quotas) goes through a `Store` bound to its id and the guild of the event and
guarded by the capability `storage`: PostgreSQL holds the data, and the module cannot name another
module's or another guild's. Work it wants done later goes through a `Scheduler` bound the same
way and guarded by the capability `schedule`: the host keeps the jobs in PostgreSQL
(`scheduled_jobs`), a runner takes the ones that are due (`FOR UPDATE SKIP LOCKED`, with a lease so
a job of a stopped bot is handed out again) and gives each to the dispatcher as an
`Event::Scheduled`, which goes to the module that scheduled it and to nobody else, through the same
lane, timeout, panic isolation and breaker as any event. Delivery is at least once; a failure
is retried after a growing pause, a module that is switched off or a full queue only postpones. Moderation cases go through a `Cases` handle bound the same way and guarded by the capability `cases`: the host numbers each guild's cases in PostgreSQL, and every module that holds the capability shares that sequence. A case is read and revoked (once) only within its own guild. Its `ApiClient` is bound to
the guild of the event: an action that targets another guild is refused, whatever capabilities
the module holds. An action that names a guild is compared directly. One that identifies only a
channel or a message (send, edit, delete, react, channel edits and permission overwrites) is
checked by looking up the channel's guild: from the cache when it knows the channel, then from
earlier answers of Discord, otherwise from Discord itself. The channels an action merely refers
to (the parent category of a new channel, the alert and exempt channels of an auto-moderation
rule) are checked the same way. A channel in another guild, in no guild (a direct message), or
whose guild cannot be determined is refused, so a failing lookup never lets an action through.
Lookups of one channel that happen at the same time make one request and share its answer, a
failure included; a failure is not remembered. Direct messages (`Action::SendDirectMessage`, guarded by the capability
`send_direct_messages`) reach only members of the guild of the event: the host checks the membership
before it opens the private channel, and a user it cannot confirm as a member gets nothing.

Answers to an interaction (`Respond`, `Followup`, `EditResponse`) are checked against a record the
host keeps: when a command arrives, before any module sees it, its token is recorded together
with the guild it came from. A module is one object shared by all guilds and could keep a client
from one guild and use it with a token it received in another; the client therefore answers only
tokens recorded for its own guild, and refuses unknown ones. The record cannot be replaced on a
client that is already bound to a guild, so a module cannot swap in a record of its own. Records
expire with the token. The table is bounded in total and per guild, so one flooded guild
cannot use up the room of the others; when a limit is reached new tokens are refused, never old
ones dropped, and a command that cannot be recorded is not handed to a module (nobody could
answer it) and counted as dropped, with a different reason for the limit of a guild and for the
total limit. A token that is recorded again keeps its first record.

## Configuration

The database stores, per guild, one sparse document (`GuildOverrides`) with a revision and a
history. At start-up every document is compiled — merged over the preset and module defaults,
checked against each module's JSON Schema, capabilities capped at what the module declares — and
installed into the `SettingsStore`. Edits through the API are validated *before* they are stored.
Other processes learn of an edit through PostgreSQL `LISTEN/NOTIFY`; because notifications are not
durable, everything is reloaded whenever the connection may have dropped some; after a failure
the resubscribe-and-reload step is retried until it succeeds. The store only
accepts an installation whose revision is at least as new as the one it holds, so concurrent
writers cannot let a stale read win. A stored document that no longer validates is skipped with a
warning and the guild keeps its previous settings.

See [configuration.md](configuration.md) for the document itself. Building, releases and the
container image are in [deploy.md](deploy.md).

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
  spawns blocking work keeps running. It is noticed and gets no new calls (see above), but the
  calls already running keep their threads, so a module that blocks in many guilds at once can
  still take every worker thread for as long as they block. Modules must not block.
- The HTTP API caps connections (256 at once), cuts off a client that takes more than 10 seconds
  to send its headers, and turns an address away for the rest of a minute after 10 failed logins.
  It has no TLS and `/readyz` queries the database on every call. Behind a reverse proxy every
  client has the proxy's address and they share one login allowance; put it behind a proxy that
  limits clients if it is reachable by anyone untrusted.
- Command events go only to the module that owns the command, not to other subscribers; clicks,
  forms and due jobs likewise go only to the module they belong to.
- `effective` configuration (`GET .../config/effective`) shows the `permissions` section as it was
  written, not resolved: the levels of the module defaults and of Administrator are not filled
  in. What is enforced is resolved correctly.
- Jobs of a guild the bot has left stay in the table; they fail (Discord refuses the module) and
  are removed two weeks after they were given up on. Nothing yet shows the jobs of a guild to its
  administrators.
- Subcommand *groups* hold subcommands only, and a command has at most two levels.
- A stored document that fails to parse makes `GET /v1/guilds/{id}/config` return 500 for that
  guild, and a replacement needs the revision from the database.
- One process per bot; shard ranges are not configurable.

## Measured

What the dispatcher does on one machine (Apple M2, 8 cores, release build, 4 worker threads, a
module that only counts, so this is the cost of the dispatcher itself and not of any real module).
Reproduce with
`cargo test --release -p closure-discord --lib measure -- --ignored --nocapture --test-threads=1`.

| | |
| --- | --- |
| Events that need no work, 64 guilds | about 3.6 million dispatched per second, 0.88 million handled per second |
| A raid on one guild, a module that needs 2 ms per event, 1,000,000 events | dispatching costs about 8 to 10 million events per second whether they are accepted or dropped |
| ... small preset (queue 200, 2 lanes) | at most 200 waiting; 258 handled, the rest dropped; drained in 0.4 s |
| ... medium preset (queue 1,000, 4 lanes) | at most 1,000 waiting; 1,128 handled; drained in 1.0 s |
| ... large preset (queue 5,000, 16 lanes) | at most 5,000 waiting; 5,608 handled; drained in 1.2 s |

The queue stays at its limit however many events arrive, and the log gets one line per ten
seconds about it, with the number of events left out. What the numbers do not say: how real
modules behave (database or Discord calls), memory use, or how the gateway connection copes
with the same load.
