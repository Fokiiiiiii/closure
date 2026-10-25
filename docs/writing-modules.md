# Writing a module

A module is a Rust crate that implements `closure_core::module::Module`. The smallest complete
example is [`crates/closure-mod-diagnostics`](../crates/closure-mod-diagnostics/src/lib.rs); the
example that uses the SDK in full (permissions, a cooldown, storage, a confirmation button, jobs
that expire things, two languages) is [`crates/closure-mod-warn`](../crates/closure-mod-warn/src/lib.rs)
with its [tests](../crates/closure-mod-warn/tests/warn.rs). Read them alongside this page.

## The short way: the SDK

Most modules are commands. With `closure-sdk` you declare a command and write what it does; the
rest below (the manifest, matching events to commands, reading options, checking permissions,
answering in the user's language) is done for you.

```rust
#[derive(CommandArgs)]
struct Warn {
    /// Member to warn          <- the doc comment is the option's description
    user: UserId,
    /// Why
    #[arg(max_length = 512)]
    reason: Option<String>,    // Option<_> = optional
}

pub fn module() -> Result<SdkModule, CoreError> {
    ModuleBuilder::new("warn", "Warnings", env!("CARGO_PKG_VERSION"))?
        .permission(PermissionSpec::new("warn.add", Level::MODERATOR, "Warn members")?)
        .command(
            Command::<Warn>::new("warn", "Warn a member")
                .requires("warn.add")                               // checked before the handler
                .cooldown(Cooldown::per_user(Duration::from_secs(5)))
                .handler(|cx, args| async move {
                    cx.reply_t("warned", &Args::new().with("user", args.user.get())).await
                }),
        )
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA)
        .build()
}
```

What the SDK does for each command:

- a user without the permission, or who has to wait for the cooldown, or whose option is invalid,
  gets a private answer in their language, and your handler does not run;
- `UserError::message("key")` from a handler answers privately with `key` of your catalog
  (`.with("name", value)` fills its variables); any other error (`?` on a Discord call) goes to the
  dispatcher, which says that something went wrong and counts the failure;
- a handler that answers nothing is answered with "Done." so Discord does not show a failure;
- the first answer is the response to the interaction, the following ones are follow-ups; they
  take turns (one is sent before the next starts), and an answer that is refused before anything
  is sent (a text over 2000 characters) leaves the interaction unanswered, so you can answer again
  with less. `cx.defer(ephemeral)` first lets the answer take longer than three seconds. Discord
  cannot make a public message private: after `cx.defer(false)` an answer for the user alone (a
  `UserError`, `reply_private`) finishes the public message with "Done." and goes to the user as a
  follow-up; use `cx.defer(true)` when the answer is private;
- on a click, `cx.defer_update()` acknowledges it and `cx.update(..)` changes the message
  afterwards; an update takes off the embeds and components the reply does not have;
- `cx.require("warn.add")`, `cx.level()` and `cx.actor()` are there for checks inside a handler
  (`require` asks about your module's own keys only);
- subcommands: `.group("warnings", "Look at warnings", [Command::<A>::new("show", ..).handler(..), ..])`.

Options are typed fields: `String`, `i64`, `f64`, `bool`, `UserId`, `ChannelId`, `RoleId`;
`#[arg(name = "..", desc = "..", min = 1, max = 5, min_length = 2, max_length = 40,
choices = ["a", "b"])]`. Required options must come before optional ones.

Buttons, menus, forms and pages: an answer carries `components` (`ui::row([ui::button(..), ..])`,
`ui::select(..)`), each with a `custom_id` that comes back when it is used. The SDK makes it
`module:route:owner:data` (`CustomId::new("confirm").only_for(user).data("42").encode(cx.module())`);
a handler for the route is `.component("confirm", |cx| async move { .. })`, where `cx.data()`
and `cx.values()` say what was clicked or chosen. The user the component was made for (`only_for`)
is the only one it works for; others are told so privately. A click that is not answered is
acknowledged, so Discord does not show a failure. Answer with `cx.update(reply)` (change the
message), `cx.reply_private(..)` or `cx.show_modal(ui::modal(..))`. A form is
`ui::modal(CustomId::new("report").encode(cx.module()), "Report", [ui::text_input(cx.module(),
"why", "Why", true, true, 200)])`, handled by `.modal("report", |cx| async move { cx.field("why") .. })`.
`Pager::new("list", page, pages).row(cx.module())` makes the back/next buttons of a list; the route's
handler reads `cx.page_requested()`. A `custom_id` has to start with your module's id, or the action is
refused: a click goes to the module it names. Discord allows 100 characters in all, so keep large
state in the module's storage and put only a key in `data`. Test with `Harness::click(Click::new(id))`
and `Harness::submit(Submit::new(id).field("module:name", "text"))`.

Reacting to events (not commands): `.on(Subscription::new(EventKind::MessageCreated).ignore_bots()
.not_in_channels([log_channel]).priority(10), |ctx, event| async move { .. Ok(Flow::Continue) })`.
The handler gets only the events that pass the filter (`in_channels`, `not_in_channels`,
`ignore_bots`, `ignore_roles`), after modules with a higher priority (default 0; equal priorities
run together). Returning `Flow::Stop` keeps the event from the modules with a lower priority, which
is how an auto-moderation module claims a message it deleted. One handler per kind of event;
declare the capabilities it needs, and the Gateway intents the events need with
`.intents(Intents::GUILD_MESSAGES)` (messages, `GUILD_MEMBERS` for members, `GUILD_MESSAGE_REACTIONS`,
`GUILD_MODERATION` for bans; members and message content are privileged and have to be switched on
for the bot in the Discord developer portal): a module that subscribes without the intent is refused
when it is built, because Discord would never send it the events. Clicks, forms and jobs are not
subscribed to; they have `component`, `modal` and `job`. In a plain module the same is
`ModuleManifest::subscribe(..)` plus `Module::on_event_flow`. `Harness::emit(event)` delivers an event as the dispatcher would.

Keeping data: declare `.capability(Capability::Storage)` and use `cx.store()` (or `ctx.store()` in
a plain module):

- entries: `set`, `set_for(key, value, ttl)` (gone after the time), `get::<T>`, `delete`,
  `list(prefix, limit)`; `replace(key, value, Expect::Revision(r), ttl)` writes only if nobody
  wrote since you read revision `r` (`entry(key)`), so two handlers do not lose each other's
  update. A revision is a number no two writes share, so one read before the entry was deleted
  (or ran out) and made again never matches the new entry; keys are 1-128 printable ASCII characters, values at most 32 KiB of JSON, at most
  10 000 entries per module and guild;
- counters: `counter_add(name, subject, delta)` returns the total, `counter_total`,
  `counter_recent(name, subject, window)` (what was added in the last minutes, up to 90 days;
  a window is whole minutes, rounded up, the current minute included), `counter_reset`; at most
  100 000 counters per module and guild.

A cooldown keeps at most 50 000 uses in memory per module (a use is forgotten when its period is
over); when that many are waiting, a user it has not seen yet is not held back, so it is a brake
against repetition and not a count you can rely on to the single use.

The data belongs to the module and the guild; nothing else can read it. Going over a limit is an
error (`StorageError`), never silent loss. Data is kept while the guild's configuration is; expired
entries and old minute counts are removed hourly. There is no separate cache: an entry with a time
to live is one.

Work for later: `.job("unmute", |cx| async move { let who: Unmute = cx.payload()?; .. Ok(()) })`
handles a job, and `cx.scheduler().job("unmute", &Unmute { user })?.key("unmute:7")
.after(Duration::from_secs(600)).await?` sets one (`cx.ctx().scheduler()` in a job or a plain
module). Declaring a job handler grants the module the capability `schedule`. A job is a name the
module knows and a JSON payload (at most 16 KiB), run once at `.after(delay)` or `.at(unix_seconds)`,
or again and again with `.every(period, first)` (at least 10 seconds; if the bot was down, the
runs it missed are skipped, not made up for). A job with a `.key(..)` is replaced when the same
key is scheduled again and `scheduler().cancel(key)` removes it; keys belong to the module and
the guild. Jobs are kept in PostgreSQL and survive a restart; at most 10 000 wait per module and
guild. A delay (`after`, the first run and the period of `every`) is at most 10 years, and a time
given to `at` is between 1970 and the end of the year 9999.

Delivery is *at least once*: if the handler returns an error, panics or times out, or the bot
stops while it runs, the job comes again (after 5 s, 10 s, 20 s, ... at most an hour), up to 8
tries, then it is kept as failed for two weeks and no longer handed out. So a handler has to be
safe to run twice; `cx.attempt()` says which try this is. A job whose name has no handler (a leftover
of an older version) is dropped with a warning, not failed. A job for a module that is switched
off in the guild, or while the guild's queue is full, waits (30 s at a time) and its tries are not
counted. The handler is called like an event handler: same timeout, same panic isolation, same
circuit breaker. Test with `Harness::run_due_jobs()` after `Harness::advance(..)`, and set up a
job with `Harness::scheduler()`.

Test with `closure_sdk::testing::Harness`: it runs a command with a given user, roles and
language against a pretend Discord, lets you change the guild's permissions and move the clock for
cooldowns and jobs, and returns what the module answered. See
[`crates/closure-sdk/tests/commands.rs`](../crates/closure-sdk/tests/commands.rs).

The sections below describe what is under the SDK, and what a module that is not only commands
(reacting to messages, members, bans) needs to know.

## 1. Describe the module (the manifest)

The manifest is data. The host reads it before running a single line of your code.

```rust
let manifest = ModuleManifest::new(ModuleId::new("warn")?, "Warnings", env!("CARGO_PKG_VERSION"))
    .description("Warn members and keep a record.")
    .intents(Intents::GUILD_MODERATION)             // gateway intents you need
    .events([EventKind::Command, EventKind::MemberLeft])
    .capabilities([Capability::SendMessages, Capability::TimeoutMembers])
    .command(/* CommandSpec */)
    .permission(PermissionSpec::new("warn.add", Level::MODERATOR, "Give a warning")?)
    .config(schema_json, default_json)              // your section of the guild configuration
    .catalog(Locale::En, EN).catalog(Locale::Ja, JA)
    .enabled_by_default();
```

- The id is `[a-z0-9_-]`, starts with a letter, at most 32 characters. It is the key under
  `modules:` in the configuration and the namespace of your messages.
- Declare only the capabilities you use. A server can narrow them, never widen them.
- Declare only the events and intents you need; the bot requests the union of all modules'
  intents, and you receive only the events you subscribe to. Slash command events are the
  exception: a `Command` event goes only to the module that owns the command, so a module does
  not need to subscribe to `EventKind::Command` to receive its own commands.
- Commands are data (`CommandSpec`): name, English description, options or subcommands (one level),
  default member permissions. Required options come before optional ones.
- Permissions are keys (`warn.add`: two to four dot-separated parts, the first being your module's
  id) with the level a member needs by default. Servers change levels and allow or deny single
  roles and users in their configuration (see `configuration.md`); you only declare the keys.
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
- **Be quick, and never block a thread.** Each call has a timeout (the guild's
  `handler_timeout_secs`), but it can only stop a module at an `await`; code that blocks or spawns
  blocking work keeps running after the timeout.
- **You act within one guild.** Your client is bound to the guild of the event. An action that
  targets another guild, or a channel or message outside it, is refused, and so is answering an
  interaction of another guild. Acting on a channel costs a lookup of its guild if the cache does
  not know it. Do not keep a client from one event for use with another guild's data.
- **Order**: events about one channel, user or role arrive in order; unrelated events may be handled
  at the same time, so share state through `Arc` and locks, not assumptions.
- Check `ctx.permissions().check(&actor, &key)` before moderation commands. The `Actor` is the
  invoker (`CommandInvocation` carries their `roles`; `administrator` comes from
  `invoker_permissions`). The decision says why it was refused, which is worth telling the user.

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
