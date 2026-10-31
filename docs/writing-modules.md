# Writing a module

A module is a Rust crate that implements `closure_core::module::Module`. The smallest complete
example, written with the SDK, is [`crates/closure-mod-diagnostics`](../crates/closure-mod-diagnostics/src/lib.rs); the
example that uses the SDK in full (permissions, a cooldown, cases, the log, direct messages,
timers, two languages) is [`crates/closure-mod-moderation`](../crates/closure-mod-moderation/src/lib.rs)
with its [tests](../crates/closure-mod-moderation/tests/moderation.rs). Read them alongside this page.

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
  afterwards; an update takes off the embeds and components the reply does not have, and it
  cannot be private (the message belongs to everyone): a private answer to a click is `reply_private`;
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

Events about roles, threads and cases:

- `RolesChanged` (`GUILD_MEMBERS`): a member gained (`added`) or lost (`removed`) roles, against the roles the bot had cached. Nothing is sent when the bot did not know the member's roles before. `MemberUpdated` is still sent for the same update.
- `ThreadCreated`, `ThreadArchived`, `ThreadUnarchived`, `ThreadDeleted` (`GUILDS`): `ThreadCreated` only for a thread that was just made; threads the bot learns of at start-up or when it is added to one are not announced. Archive and unarchive compare with the cached state, so nothing is sent when that state is unknown. A thread counts as part of its parent channel for `in_channels` and `not_in_channels`. These events carry no roles, so `ignore_roles` does not apply to them.
- `CaseCreated`: a case recorded in the guild by any module, delivered once after it was stored. The module that recorded it gets it too, so compare `case.module` with your own id. A failing handler does not fail the recording. A case recorded while a `CaseCreated` handler runs is stored but not announced again, so announcements never chain; the case quota is not used up by such a loop.

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

Cases: a moderation record (warning, timeout, kick, ...) numbered per server. Declare `.capability(Capability::Cases)` and use `cx.cases()` (or `ctx.cases()`): `create(NewCase { kind, target, moderator, reason, expires_at })` returns the `Case` with its number; `get(number)`, `for_target(user, before, limit)` and `recent(before, limit)` list cases newest first; `revoke(number, by, reason)` revokes a case, once. The numbers belong to the server and every module that holds `cases` writes into the same sequence, so a warning and a timeout from two modules are numbered together. A module sees only its own server's cases. A kind is 1 to 32 of `a-z`, `0-9`, `_` and `-`; a reason is at most 512 characters. Tests read what a module recorded with `Harness::cases()`.

ModLog: `cx.modlog(&case)` (or `job.modlog(&case)`) posts a case to the server's log channel as an embed. The embed is in the server's language, not the invoker's, since moderators read the log. With no log channel it does nothing and returns `Ok(None)`. The module needs the capability `SendMessages` as well as `cases`. A revoked case is posted in grey, with who revoked it and why. Tests set the channel with `Harness::set_log_channel(id)` and read the embeds with `Harness::sent_embeds()`.

Direct messages: `cx.direct_message(user, DirectMessage::text("..."))` (or `DirectMessage::embed(embed)`) sends a private message. The module needs the capability `send_direct_messages`, which is separate from `send_messages`. The user must be a member of the server; the host checks that before sending, and anyone else is refused. `Sent::Closed` means the user does not accept direct messages from the bot: that is an answer, not an error, and the module should carry on. Discord may limit a bot that sends many direct messages in a short time, so send them sparingly, one notice per action rather than one per message.

A `Duration` field is a command option typed as words: `#[derive(CommandArgs)] struct Mute { user: UserId, time: Duration }`
accepts `10m`, `1h30m`, `1h 30m`, `2d` or `1w2d` (units `s m h d w`, lowercase; at most ten years).
Anything else is refused with the usual bad-argument message before the handler runs. The same
parser is `closure_sdk::duration::parse`. To show a length, use `cx.format_duration(duration)`
(in the user's language, the two largest units, e.g. "2 days 3 hours"). To show a moment that
Discord renders in each reader's own time zone, use `duration::timestamp(unix, TimeStyle::Relative)`.

A `Span` field is a command option that takes a length as above or a moment, such as `2026-11-01 18:00`
or `18時`: `#[derive(CommandArgs)] struct Mute { user: UserId, until: Span }`. The text is checked when
the command arrives, so a typo is refused as a bad argument. The handler resolves it with
`span.resolve(&cx)?`, which reads the clock of the command and returns the length from now, or
`UserError` (not understood, in the past, more than ten years away). A moment without a zone is read in the
server's `timezone` when it has one, else at UTC+09:00 on a Japanese server and at UTC otherwise (see the
moderation guide). `Span::resolve_at(now,
offset)` is the same without a `Cx`, for tests. `Span::from_duration` makes a span from a `Duration`.

An embed (the box under a message) is built with `EmbedBuilder`, which checks Discord's limits when
you call `build()` (characters, not bytes; the whole embed at most 6000), so a too-long embed is an
`EmbedError` that the SDK reports as a module failure instead of a message Discord refuses:

```rust
let embed = EmbedBuilder::new()
    .title("Warning")
    .description(embed::escape(&reason))
    .field("Member", member, true)
    .color(Color::Warning)
    .build()?;
cx.reply_with(InteractionReply { embeds: vec![embed], ..InteractionReply::default() }).await
```

Text a user wrote should go through `embed::escape` (Markdown and mentions are made harmless) before
it is shown, and `embed::truncate(text, max_chars)` shortens text to a limit, ending it with `…`.

Before a moderation action on a member (kick, ban, timeout), call `cx.check_target(user).await?`.
It refuses, in the user's language, the invoker themselves, the server owner, bots, and members whose
highest role is not below the invoker's; the owner of the server is exempt from that last rule, and
someone who is not a member passes. It reads the members and roles of the guild, so the module
declares `Capability::ReadGuild`.

Test with `closure_sdk::testing::Harness`: it runs a command with a given user, roles and
language against a pretend Discord, lets you change the guild's permissions and move the clock for
cooldowns and jobs, and returns what the module answered. See
[`crates/closure-sdk/tests/commands.rs`](../crates/closure-sdk/tests/commands.rs).

### Punishing from a module

`closure_sdk::punish::Punisher` does what the `moderation` module does when a moderator punishes: the
Discord action, the case, the log post, the direct message to the member, and the timer that ends a
timed punishment. The caller checks what its own users may do (`cx.require`, `cx.check_target`) and
reads its settings into a `Settings`:

- `dm`: whether the member is messaged;
- `timeout` and `voice`: a `punish::Method`, Discord's own feature or a role. With `Method::Role`,
  `timeout_role` and `voice_role` hold the role's id.

```rust
let punisher = Punisher::new(cx.ctx(), Moderator::of(cx.user()), settings)
    .reply_locale(cx.locale());
let done = punisher.ban(user, Some(Duration::from_secs(86_400)), 0, reason).await?;
```

`Moderator::bot(ctx, name)` names the bot for a module that acts without a person, such as automod.
`reply_locale` sets the language of the notes that come back in `Done::notes` (read them with
`note_text`). The direct message and the log use the server's language.

The methods are `warn`, `timeout`, `lift_timeout`, `kick`, `ban`, `unban`, `voice_mute` and
`voice_unmute`. Each records one case and returns it; a lift also revokes the newest case of its kind
that still stands, or after `undoing(case)` the case it names. While a newer case of that kind stands, a lift
after `undoing(case)` does nothing and returns the named case still standing, for the caller to revoke. A timer
waits while a newer case of its kind stands against the member: a role lift is set again for the latest end of
the others, and a ban lift does nothing. A punishment that could not do all of it (the direct message did not arrive, the
timer could not be set) still goes through, and `notes` says so. Escalating on counts of warnings is
the module's own decision.

`punish::jobs(builder)` registers the handlers of the three timers (`unban`, `untimeout`, `vc_unmute`)
under the names and payloads that scheduled rows already use. Call it once on the builder of a module
that punishes with timers, and do not register those names again.

Declare the capabilities of what the module carries out: `Cases`, `SendMessages` (the log),
`SendDirectMessages`, `KickMembers`, `BanMembers`, `TimeoutMembers`, `ManageRoles`, `MuteMembers` and
`ReadGuild`. `punish::jobs` adds `Schedule`.

`Settings::strip` (`punish::Strip`) takes the member's roles off when the dedicated role is given, and gives
the ones taken off back when the punishment ends (`RoleSelection::{None, All, Some(ids)}`). Its roles are
remembered with the module's `Storage` capability; without it nothing is taken off.

Dedicated roles of the role method: `punish::ensure_role(ctx, RestrictionRole::Timeout | Voice, configured)`
returns the role a restriction gives. A configured role that exists is used as it is and never touched.
Otherwise the remembered or a same-named role (`Timed out`, `Voice muted`) is used when it is safe to hand
out (no permissions, not managed, not @everyone, below the bot's top role); an unsafe one is ignored. A new
role is created with no permissions. Calls for one guild and kind take turns within a process. A created
role gets a deny overwrite in every channel by a job that `punish::jobs` registers (`restriction_sync`), which
keeps the other overwrites of the channel. A module that wants the overwrites on new channels calls
`punish::channel_created(ctx, channel)` on `Event::ChannelCreated`; it sets only the role in use. The role needs the capabilities
`ManageRoles` and `ManageChannels`, and the job needs `Schedule`.

### Configuration changes

`Event::ConfigChanged { guild_id }` (kind `config_changed`, no Discord intent) reaches every enabled
module of a guild right after the guild's configuration was reloaded and changed, and once for each
guild that has a configuration when the bot sees the guild at start-up or when it is added. An identical
reload sends nothing. Delivery is best effort: a refused hand-over is retried a few times, then dropped
with a warning. The same change can also arrive twice (a reload racing a guild's arrival), so handlers
must tolerate duplicates. Use it to prepare what the settings need, such as the roles of the role method.

### Listing channels

`Action::ListChannels { guild_id }` answers `Outcome::Channels(Vec<Channel>)`, the channels of the guild
with their permission overwrites. It needs `ReadGuild`.

### Replies

A reply is one embed whose description is the text. `reply` and `reply_private` use the `Info`
tone; `say`, `say_private`, `say_t` and `say_private_t` take a `Tone` (`Info`, `Success`, `Warning`
or `Error`) for the colour of the bar. Empty text is refused as before.

A `Success` reply starts with the server's `success_emoji` and an `Error` reply with its
`error_emoji` (see [configuration](configuration.md#time-zone-prefix-and-emojis)), followed by a space;
`Info` and `Warning` get none. `cx.tone_text(tone, text)` gives the text as the reply shows it, for a
reply built by hand. Tests set the policy with `Harness::set_policy(GuildPolicy { .. })`.

Errors the SDK answers (a `UserError` such as a missing permission or a cooldown) are `Error`
embeds and always private, whatever the setting below says.

The administrator chooses per module with the `replies` key: `"private"` makes every reply and
deferral ephemeral (only the user sees it), `"public"` makes them public, and when the key is unset
each reply keeps its own visibility. The SDK adds the key to every module's schema, so a module must
not declare it itself. `cx.config()` leaves the key out before it reads the module's own type.

The sections below describe what is under the SDK, and what a module that is not only commands
(reacting to messages, members, bans) needs to know.

### Option types, groups, autocomplete, context menus, expiring buttons

Options: `Attachment` (the file uploaded with the command: id, filename, size, content type and url)
and `Mentionable` (a `Mentionable::User(id)` or `Mentionable::Role(id)`) are field types like the
others.

```rust
#[derive(CommandArgs)]
struct Keep {
    /// The file to keep
    file: Attachment,
    /// Who it is about
    who: Option<Mentionable>,
}
```

Groups, two levels deep (`/warn list all`). Each subcommand has its own permission and cooldown; a
group has no options of its own. `ModuleBuilder::group` (one level) still works.

```rust
ModuleBuilder::new("warn", "Warnings", env!("CARGO_PKG_VERSION"))?
    .groups("warn", "Warnings", [
        SubcommandGroup::new("list", "Look at warnings")
            .subcommand(Command::<()>::new("all", "Every warning").handler(list_all)),
    ])
```

A command may take plain subcommands and groups together: call `group` and `groups` with the same
name, in either order (`/warn status` next to `/warn list all`). The calls add to one command, and
the description of the first one stands. The build refuses a name used twice among a command's
subcommands and groups, a plain command with options or without them that also takes subcommands,
and more than 25 subcommands and groups.

Autocomplete: mark a text or number option with `#[arg(autocomplete)]` (or name it in
`ModuleBuilder::autocomplete("warn", "reason", handler)`, which marks it too). The handler gets an
`AutocompleteCx` (`focused()`, `focused_option()`, `option(name)` for the options already filled in,
`guild_id()`, `config()`, `t()`, `store()`) and returns `Vec<Choice>`; `Choice::new(name, value)`
takes a string, an integer or a number as the value. Discord takes at most 25 choices of at most 100
characters, and a longer answer is logged and answered with none. A user who may not run the command
gets none, and so does a handler that fails. The answer is due within three seconds: the dispatcher
gives the handler 2.5 seconds, then answers with none.

```rust
.autocomplete("warn", "reason", |cx| async move {
    let typed = cx.focused().to_owned();
    Ok(REASONS.iter().filter(|r| r.starts_with(typed.as_str()))
        .map(|r| Choice::new(*r, *r)).collect())
})
```

Context-menu commands on a user or on a message: `ContextCommand::user("name")` or
`ContextCommand::message("name")`, with `requires` and `cooldown` as for a command, and `.handler`.
`ModuleBuilder::user_command("name", handler)` and `message_command` are the shorthands without them.
The handler reads `cx.target_user()` or `cx.target_message()` (one of them is `None`). Discord takes
a name of 1 to 32 characters and no description; a module has at most five of each kind (checked when
the module is built and when the registry is made).

Expiring buttons: `CustomId::new("go").expires_in(cx.now(), Duration::from_secs(600))` puts the
expiry into the `custom_id`, so it lasts as long as the message. A press or a submission after it is
refused privately with `sdk-expired`. `Pager::expires_in` does the same for the page buttons. The
SDK does not take the dead buttons off the message: an update would clear its embeds too, and the
interaction does not carry them.

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
- Commands are data (`CommandSpec`): name, English description, options, or subcommands (in groups, at most two levels),
  default member permissions. Required options come before optional ones.
- Permissions are keys (`warn.add`: two to four dot-separated parts, the first being your module's
  id) with the level a member needs by default. Servers change levels and allow or deny single
  roles and users in their configuration (see `configuration.md`); you only declare the keys.
- The registry refuses duplicate ids, unknown or cyclic dependencies and colliding commands at
  start-up.

### Hints for the dashboard (`x-ui`)

A property of the configuration schema may carry an `x-ui` object that the dashboard uses to draw
its form. The bot checks its shape at start-up and accepts no other keys in it:

```json
"x-ui": { "widget": "role", "group": "Limits", "order": 1, "help": "Who may use the command" }
```

- `widget`: one of `role`, `channel`, `duration`, `textarea` or `user`;
- `group` (at most 40 characters) and `help` (at most 200): text for the form;
- `order`: an integer from 0 to 1000;
- `level`: `basic` or `advanced`, which view the setting belongs to (see below).

Other `x-` keywords are passed on unchanged. The module reference in `docs/reference` shows the widget.

### Simple and advanced settings (`x-ui.level`)

A module marks the few settings a simple form should show with `"level": "basic"`:

```json
"timeout_role": { "type": ["string", "null"], "x-ui": { "level": "basic", "widget": "role" } }
```

- A setting without a level is advanced. A setting under a basic one is basic too, and a basic
  setting may not have an advanced one below it (the bot refuses that at start-up).
- Only properties written inline under `properties` are looked at. A property that is a `$ref` is
  one setting as a whole, and a marker inside a definition under `$defs` is not read.
- The guild-wide keys (`language`, `timezone`, `prefix`, `preset` and the others in
  `closure_config::BASIC_KEYS`) and each module's `enabled` switch are basic without a marker.
- Keep a marked setting a simple, scalar choice that a form can show alone. Templates, escalation
  rules and similar lists stay advanced.

The API serves both views (see [configuration](configuration.md#simple-and-advanced-views)).

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

Add it to `modules()` in [`crates/closure/src/lib.rs`](../crates/closure/src/lib.rs) and add the
crate to `crates/closure/Cargo.toml`. That is the only place that knows the full list.
`cargo run -p closure-xtask -- new-module <id>` creates the crate from the templates and prints both
lines. The tests in [`app.rs`](../crates/closure/src/app.rs) check that the whole set forms a valid
registry and that every command is publishable in every language. After adding a module, run
`cargo run -p closure-xtask -- docs` to write its page in `docs/reference`.

## 5. Test it

You do not need Discord or a database:

- `closure_core::testing::MockApi` records the actions your module asks for and can be told to fail;
- `StaticSettings` provides fixed per-guild settings;
- `EventContext::new(...)` builds the context a handler receives;
- `Dispatcher` (in `closure-discord`) can run modules end to end against a `MockApi`.

See the `tests` module of the diagnostics crate for a complete set written with the SDK's
`testing::Harness`: replies in each language, configuration, failed calls, manifest validity and
catalog parity.

## Reading messages

A module that reads messages subscribes to `EventKind::MessageCreated`, `MessageUpdated`,
`MessageDeleted` or `MessagesBulkDeleted`. Created and updated events carry a `MessageInfo`: the
guild, channel, author and their roles, content, mentioned users and roles, attachments, number of
embeds and stickers, the message it replies to, whether a webhook sent it, and the times it was
sent and last edited. Deleted events carry the ids only, since the message is gone.

What differs on an update:

- `created_at` is when the message was sent, also on an update. `edited_at` is set once it was
  edited.
- Twilight reads an update as a whole message. A field its payload leaves out comes out empty, and
  a payload without the fields Discord always sends (an embed-only update, for instance, when a link
  preview loads) is not delivered at all.
- `content` is empty without the content intent (below). `author_roles` is empty when there is no
  member data, as for a webhook's message.

`content` needs the privileged `MESSAGE_CONTENT` intent. A module that reads it declares that:

```rust
ModuleBuilder::new("automod", "Automod", env!("CARGO_PKG_VERSION"))?
    .intents(Intents::MESSAGE_CONTENT)
```

The intent must also be switched on for the bot in the Discord Developer Portal, otherwise
`content` is empty. The bot asks Discord for the union of the intents of all modules, so a module
that declares it makes the bot require it, in every guild.

`closure_sdk::text` has plain-text helpers (no I/O), each documented with an example:

- `normalize(text)`: lower case, fullwidth ASCII folded, zero-width and combining marks removed,
  whitespace collapsed. It is not full Unicode NFKC.
- `WordList::new(words, MatchMode::Contains | MatchMode::Whole)`, with `find` (the first listed
  word that matches) and `find_all`. `Whole` needs no letter or digit next to the match, so `ass`
  does not match `class`. Japanese has no spaces, so use `Contains` for it.
- `links(text)` gives each `Link { url, host }`; `link.is_domain("example.com")` is true for the
  domain and its subdomains only.
- `invites`, `mention_count`, `caps_ratio`, `longest_run`, `emoji_count` and `combining_run`.

```rust
use closure_sdk::text::{MatchMode, WordList, links};

let banned = WordList::new(["spam"], MatchMode::Whole);
let spam = banned.find(&message.content);
let foreign = links(&message.content).iter().any(|link| !link.is_domain("example.com"));
```

In a test, `Harness::message(Message::new(author, channel).content("…"))` delivers a message the
way the dispatcher does, and returns what the module's filter let through.

## Message history

A module that looks at earlier messages (spam by author, cleaning a user's recent messages, logs of
edits and deletions) declares `Capability::ReadMessageHistory`, the capability `message_history`.
It reads the history with `cx.history()` in a command, or `ctx.history()` in an event handler:

- `by_author(user, since, limit)` and `in_channel(channel, since, limit)` list messages newest
  first, sent at or after `since` (unix seconds). `limit` is clamped to 1 to 100.
- `get(channel, id)` returns one message.

Each result is a `SavedMessage` with the author, the content, attachments, mentions, reply target,
and the times it was sent, edited and deleted. A deleted message stays in the history with
`deleted_at` set, so a module can see what was removed. The handle also has `save` and
`mark_deleted`, which the host uses to record what Discord sends.

What is kept:

- The host records created, edited and deleted messages (bulk deletes too), bot messages included.
  It records only in guilds where some enabled module holds `message_history`. Where none does,
  nothing is recorded.
- Retention is set with `--message-history-days` (`MESSAGE_HISTORY_DAYS`), 7 by default. Older
  messages are removed by the storage upkeep. `0` keeps nothing.
- An edit replaces the stored text. An update that carries no text keeps the stored text, because
  Discord sends none without the content intent.
- Without the content intent (see "Reading messages") new messages have empty text in the history.
  Nothing is invented for them.
- A module only sees the messages of its own guild.

In a test, `Harness::seed_message(SavedMessage)` puts a message into the history of the harness guild,
and `harness.history()` reads it back with the module's own capability.

## Recent actions

`closure_sdk::recent::RecentActions` keeps what happened in the last moments (messages, mentions,
joins) so that a module can count a burst and act once. It is in memory only: a restart starts
empty. Keep one in the module's state; a clone shares the same store.

- `record(guild, Recent { kind, subject, group, count, at, messages, tag })` stores an action.
  Actions older than `RecentLimits::max_age` (default one hour) before the newest one are dropped,
  and so are the oldest past `max_entries_per_guild` (default 20,000).
- `sum`, `entries` and `take` look at one kind (and one subject, or all) in the last `within`.
  A window starts inclusively: an action exactly `within` old counts. `take` removes what it
  returns, so a burst trips a rule once.
- `RecentFilter` narrows to one group (such as a channel) and can require the window to span
  enough distinct groups or subjects. When it does not, `sum` is 0 and `take` returns nothing.
- `mark_handled(guild, key, until)` and `already_handled(guild, key, now)` let later actions of the
  same burst pass without triggering again, until `until`.

## Counters

`closure_sdk::counters::Counters` keeps named integers that a module declares in its config as
`CounterDef`s: one global value, or one per channel and/or per user (`per_channel`, `per_user`).
Values are kept between `0` and `2^31 - 1`; `add` and `set` clamp into that range.

- `Decay { amount, every }` moves the value by `amount` for each whole period since the value was
  last written. It is applied when the value is read or changed, so no job runs. A period that has
  not passed yet is kept for later.
- A `TriggerDef` has a `Condition` such as `>= 5` (`>`, `<`, `=`, `!=`, `<=` too) and a reverse,
  such as `< 5`. A change fires a trigger when the condition did not hold for the value as last
  written (before decay) and holds for the new one. Its reverse fires when the condition held for
  the stored value and the reverse holds for the new one. So a value that jumps over the reverse's
  range fires nothing; a later step across it does. `reset` and `reset_all` fire nothing.
- Each change is a compare-and-set write of the scope's entry through the module's `Store`. A
  conflict is retried up to five times, then reported as `CounterError::Conflict`. Needs the
  `storage` capability.

Shortcut: a trigger that only decay crosses fires on the next change of that scope, not when the
decay happens, and a value nobody has written yet reads as its initial value.

## Templates

`closure_sdk::template` renders text from values a module gives it: `Template::parse(text)` once,
then `render(&values)`, or `template::render(text, &values)` in one step.

- `{name}` and `{a.b.c}` are replaced by the value at that path. A name the values do not have
  renders as nothing. A list shows its items joined by commas; a map shows nothing, so name one of
  its fields.
- `{{` and `}}` are literal braces. A lone `}`, and a `{` that is never closed or sits inside
  another variable, are errors with their character position.
- There are no functions. A render stops at 4,000 characters.
- `template::values::user`, `guild` and `message` build the usual maps (ids are text, as in
  Zeppelin); `Values::from_json` takes a JSON object of data.

## More actions and their capabilities

Besides moderation, a module's `ApiClient` can do the following. Each call needs the capability in brackets, and a client bound to a guild refuses anything that reaches outside that guild, including channels that an action only refers to.

- Messages: read one, the history and the pins, and who reacted [`read_messages`]; pin, unpin, crosspost, remove one user's reaction or all of them [`manage_messages`].
- Voice: deafen [`deafen_members`]; move a member into a voice channel or out of voice [`move_members`].
- Threads: create, edit [`manage_threads`]; join and leave [`send_messages`]; add or remove a member [`manage_threads`]; list members [`read_messages`].
- Invites: create, list, delete [`manage_invites`].
- Webhooks: create, list, delete [`manage_webhooks`]; post through one [`send_messages`]. A webhook token is redacted from debug output.
- Members and guild: search and list members [`read_guild`]; prune [`kick_members`]; edit the guild [`manage_guild`]; reorder roles [`manage_roles`] or channels [`manage_channels`]; open a direct message channel [`send_direct_messages`].
- Scheduled events: list [`read_guild`]; create, edit, delete [`manage_events`]. Emoji: list [`read_guild`]; create, delete [`manage_emojis`].

## Text commands

A message that starts with the guild's `prefix` (see [configuration](configuration.md)) runs the
slash command of the same name, with the same handler, checks, cooldowns and limits. Nothing is
registered for it: a module needs no extra code.

- **Syntax.** `!warn @user spam`: the prefix, the command name (case-insensitive), then the words.
  A subcommand or group is named by the next one or two words: `!automod rule add name`.
- **Options** take the words in their declared order. A user, role, channel or mentionable is written
  as a mention (`<@5>`, `<@!5>`, `<@&9>`, `<#7>`) or as a bare id. Integers, numbers and booleans
  (`true`, `yes`, `on`, `1` and their negatives) are parsed by kind. Choices, ranges and lengths are
  enforced; a choice is given to the handler in its own spelling, whatever the case typed.
- **Quotes** keep spaces inside one value: `!timeout @user "2 hours" being rude`. A backslash takes
  the next character literally. A quote that is never closed runs to the end of the line.
- **Rest of the line.** The last option of a command, when it is a string and not quoted, takes
  everything that is left, verbatim: `!warn @user being rude` gives the reason `being rude`. So a
  duration or span with spaces needs quotes unless it is the last option.
- **Attachments** come with the message, in order; a text command takes no words for them.
- **Usage.** Words that do not fit (a missing required option, a bad value, extra words) are answered
  with a short usage line such as `Usage: !warn <user> [reason]`, in the guild's language, and nothing
  runs. A command word that names no enabled command is ignored: the message is just chat.
- **Replies.** A reply is sent as a reply to the message that asked, with no mentions. `defer` sends
  nothing, and the first `edit_response` sends the reply; later edits change it (for 15 minutes).
  An `ephemeral` reply cannot be private in a channel: it is deleted after 30 seconds.
- **Forms.** A modal cannot be shown to a text command. The module gets its answer, and the author is
  told in the guild's language to use the slash command.
- **What the host does not know.** The invoker's permissions are computed from the cache: the owner
  and administrators are known, and channel overwrites are applied, but the roles come from the
  message, so a message without member data counts as holding only `@everyone`. A thread is judged by
  its own overwrites, not its parent's. Text commands read the message content, which needs the
  `MESSAGE_CONTENT` intent; automod declares it, so it is requested whenever automod is loaded.

## Later: scripts

A Lua or WebAssembly module would supply the same manifest from a file and exchange the same
serialisable events and actions. Scripts start with no capabilities until a server administrator
grants them. See [architecture.md](architecture.md#extending-lua-and-webassembly).
