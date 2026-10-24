# Writing a module

A module is a Rust crate that implements `closure_core::module::Module`. The smallest complete
example is [`crates/closure-mod-diagnostics`](../crates/closure-mod-diagnostics/src/lib.rs); read
it alongside this page.

## 1. Describe the module (the manifest)

The manifest is data. The host reads it before running a single line of your code.

```rust
let manifest = ModuleManifest::new(ModuleId::new("warn")?, "Warnings", env!("CARGO_PKG_VERSION"))
    .description("Warn members and keep a record.")
    .intents(Intents::GUILD_MODERATION)             // gateway intents you need
    .events([EventKind::Command, EventKind::MemberLeft])
    .capabilities([Capability::SendMessages, Capability::TimeoutMembers])
    .command(/* CommandSpec */)
    .config(schema_json, default_json)              // your section of the guild configuration
    .catalog(Locale::En, EN).catalog(Locale::Ja, JA)
    .enabled_by_default();
```

- The id is `[a-z0-9_-]`, starts with a letter, at most 32 characters. It is the key under
  `modules:` in the configuration and the namespace of your messages.
- Declare only the capabilities you use. A server can narrow them, never widen them.
- Declare only the events and intents you need; the bot requests the union of all modules'
  intents, and you receive only the events you subscribe to.
- Commands are data (`CommandSpec`): name, English description, options or subcommands (one level),
  default member permissions. Required options come before optional ones.
- The registry refuses duplicate ids, unknown or cyclic dependencies and colliding commands at
  start-up.

## 2. Handle events

```rust
#[async_trait]
impl Module for WarnModule {
    fn manifest(&self) -> &ModuleManifest { &self.manifest }

    async fn on_event(&self, ctx: &EventContext, event: &Event) -> Result<(), ModuleError> {
        let Event::Command(invocation) = event else { return Ok(()) };
        let config: Config = ctx.parse_config()?;               // typed, from the guild's section
        let text = ctx.t_in(ctx.reply_locale(invocation), "warned", &Args::new().with("user", name));
        ctx.api.respond(invocation, InteractionReply { content: Some(text), ..Default::default() }).await?;
        Ok(())
    }
}
```

What the context gives you: `ctx.api` (the capability-gated Discord client), `ctx.guild_id`,
`ctx.locale` (the guild's language), `ctx.reply_locale(invocation)`, `ctx.config()` /
`parse_config()`, `ctx.policy()` (log channel and moderator roles of the guild) and `ctx.t*()` for
translations.

Things to know:

- **Moderation actions need a reason**, which ends up in Discord's audit log: `Reason::new("…")` or
  `Reason::truncated(text)` for generated text.
- **Messages never ping anyone** unless you list the users or roles in a `MentionPolicy`.
- **Answer slash commands within three seconds**, or `defer` first and `followup` later.
- **Return errors, do not swallow them.** A `DiscordError` becomes a localised answer to the user
  without internal details; other errors are logged and counted. A module that keeps failing in a
  guild is paused there for a while.
- **Be quick.** Each call has a timeout (the guild's `handler_timeout_secs`). Do not block threads.
- **Order**: events about one channel, user or role arrive in order; unrelated events may be handled
  at the same time, so share state through `Arc` and locks, not assumptions.
- Check `ctx.policy().is_moderator(&roles)` (and the invoker's Discord permissions) before
  moderation commands.

## 3. Add messages in every language

Catalogs are [Fluent](https://projectfluent.org) files. By convention (used when publishing slash
commands):

```ftl
command-warn = warn
    .description = Warn a member
option-warn-user = user
    .description = Member to warn
subcommand-warn-list = list
warned = { $user } was warned.
```

Write the English and Japanese file together. A test in your crate should assert that both define
the same messages (`closure_i18n::missing_keys`), as the diagnostics module does. A missing
translation falls back to English rather than failing, but it is still a bug.

## 4. Register the module

Add it to `modules()` in [`crates/closure/src/app.rs`](../crates/closure/src/app.rs). That is the
only place that knows the full list. A test there checks the whole set forms a valid registry and
that every command is publishable in every language.

## 5. Test it

You do not need Discord or a database:

- `closure_core::testing::MockApi` records the actions your module asks for and can be told to fail;
- `StaticSettings` provides fixed per-guild settings;
- `EventContext::new(...)` builds the context a handler receives;
- `Dispatcher` (in `closure-discord`) can run modules end to end against a `MockApi`.

See the `tests` module of the diagnostics crate for a complete set: replies in each language,
configuration, ignoring unrelated events, invalid configuration, manifest validity and catalog
parity.

## Later: scripts

A Lua or WebAssembly module would supply the same manifest from a file and exchange the same
serialisable events and actions. Scripts start with no capabilities until a server administrator
grants them. See [architecture.md](architecture.md#extending-lua-and-webassembly).
