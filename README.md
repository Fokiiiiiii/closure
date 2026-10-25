# closure

A Rust-based Discord bot for servers of all sizes. · [日本語](README.ja.md)

closure is a moderation bot built around a small, strict core. Everything a feature needs —
events, Discord actions, per-server configuration, translations — is provided by that core, so
new features are modules that cannot weaken it.

**Status:** the foundation is in place (core, configuration, Discord layer, HTTP API, one
example module). The moderation features themselves are the next step.

## What the foundation gives you

- **Modules with a narrow door.** A module declares what it needs (events, gateway intents,
  capabilities, commands, configuration schema, message catalogs). It never touches Discord
  directly; it asks through a client that checks its capabilities and validates every request
  first. See [docs/writing-modules.md](docs/writing-modules.md).
- **Two ways to configure, one document.** Servers can be set up with a preset and a few
  switches, or with a full YAML file. Both edit the same sparse document; only what you change is
  stored, so improved defaults reach everyone. See [docs/configuration.md](docs/configuration.md).
- **Built for scale and for small servers.** Events are handled in per-server lanes that keep
  order where it matters and run in parallel elsewhere, with bounded queues, timeouts, panic
  isolation and a circuit breaker per module. Presets size those limits.
- **Safe Discord actions.** Every action is plain data with a required audit-log reason, checked
  against the role hierarchy before it is sent. Messages never ping anyone unless asked to.
- **English and Japanese**, with more languages a catalog away. Each server chooses a language
  (or follows Discord's), and command replies use the language of the person who ran them.
- **Operations.** Health and readiness probes, Prometheus metrics, structured logs, graceful
  shutdown, a database that is the single source of truth.

How the pieces fit: [docs/architecture.md](docs/architecture.md).

## Running it

You need a recent stable Rust toolchain and PostgreSQL 14 or newer.

1. Create an application and a bot in the [Discord developer portal](https://discord.com/developers/applications).
   Invite it with the `bot` and `applications.commands` scopes. Under *Bot*, switch on the **Server
   Members Intent** (a privileged intent; the moderation module needs it for rejoining members).
2. Create an empty database, e.g. `createdb closure`.
3. Provide the settings as environment variables (a `.env` file in the working directory is read
   too; keep it out of version control):

   ```text
   DISCORD_TOKEN=...            # bot token
   DISCORD_APPLICATION_ID=...
   DATABASE_URL=postgres://user:password@localhost/closure
   API_ADMIN_TOKEN=...          # optional, at least 24 characters; without it the HTTP API is off
   COMMAND_GUILD=...            # optional, publish commands to one server only (instant)
   ```

4. `cargo run --release -p closure`

Migrations are applied on start. `closure --help` lists every option. Stop it with Ctrl-C or
SIGTERM; it finishes queued work first.

The HTTP API listens on `127.0.0.1:8080` by default. If you expose it, put TLS in front of it.

## Development

```text
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
DATABASE_URL=postgres://localhost/closure_test cargo test --workspace
cargo deny check
```

The database tests create and drop their own databases, so `DATABASE_URL` must point at a server
where the user may do that. Tests against the real Discord API are ignored by default; they use a
test application and server you provide (see the header of
`crates/closure-discord/tests/live.rs`).

## Limitations

- Modules are Rust crates compiled in. The contract (manifests, serialisable events and actions,
  capabilities) is designed so Lua and WebAssembly modules can be added later, but they are not
  implemented.
- One process per bot. Running several shard groups in separate processes is not supported yet.
- The HTTP API is protected by a single admin token; sign-in for server administrators (Discord
  OAuth2) belongs to the future dashboard.
- Configuration problems are reported with stable codes and English messages; translating them
  is not done yet.

## License

[GNU Affero General Public License v3.0](LICENSE) (`AGPL-3.0-only`). If you run a modified
version for others over a network you must offer them its source; `/about` points to this
repository.
