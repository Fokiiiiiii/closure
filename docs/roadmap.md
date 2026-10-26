# Roadmap of the module platform

The rule for what goes in: **a layer that might be used is better in than out.** Modules are only
as good as the platform under them, so the platform covers Discord widely, even where no module
needs a piece yet. Every piece comes with tests that were seen failing first, and the same audit
as the rest.

State at the time of writing: the SDK has commands (derive macro, subcommands, cooldown,
permission levels), configuration in three layers, storage and counters, event subscriptions,
buttons, menus, forms and pages, jobs, durations, a check of who a moderation may act on, and
the reference module `closure-mod-moderation`. Done as libraries for the automod rules: recent
actions (`closure_sdk::recent`), counters with triggers (`closure_sdk::counters`) and a small
template language (`closure_sdk::template`); see [writing modules](writing-modules.md).

## Order of work

| Step | What | Why first |
| --- | --- | --- |
| M9a | `closure_sdk::embed`: an embed builder that checks Discord's limits | ModLog and most modules |
| M9b | Cases: records of moderation, numbered per guild, shared by modules (host service, capability `cases`) | moderation and automod share them |
| M9c | ModLog: posts a case to the log channel in the guild's language | uses M9a and M9b |
| M9d | Direct messages to a user (`Action::SendDirectMessage`), with the refusals Discord gives | warn/ban notices |
| — | **`moderation` module**: ban, kick, timeout, warn, cases, log (all done: warn, note, cases, log, timeout, kick, ban with timed bans and unban, and escalation of warnings; see [moderation](moderation.md)) | first real module |
| — | **Moderation, stage 1** (done): embed replies and the `replies` setting for every module, timeouts and voice mutes by Discord or by a role, templates when a reason or duration is left out, undo buttons, a history line, rejoin re-application. Still to come: autocomplete of templates and context-menu commands (M11) | the owner rewrites the moderation commands later; the platform parts stay |
| — | **Automod skeleton** (done): the `automod` module with the frame its rules run in: nine checks, scope, actions, dry run, notify and `/automod status` (see [automod](automod.md)). Next: the owner specifies the detailed rules; strikes; the message history and cache of M10 | the rules come later, the frame stays |
| M10 | Message kit: richer `MessageInfo`, text matching helpers, the content intent: **done** (see [writing modules](writing-modules.md#reading-messages)). Message history: **done** (see [writing modules](writing-modules.md#message-history)). Still to do: optional bounded cache | automod and logging |
| M11 | Interaction surface: **done** (below; the clearing of expired buttons is not) | convenience for every module |
| M12 | Events and actions that Discord has and we do not (below) | breadth |
| M13 | Services: user profile, templates, audit trail for modules, HTTP client, metrics, feature switches | breadth |
| M14 | Tooling: compile-fail tests for the macro, button expiry, `/permissions explain`; done: docs generator (`cargo run -p closure-xtask -- docs`), module scaffold (`new-module`), x-ui hints | quality |
| later | Workflow, module-to-module calls, hot reload (Lua/WASM); real time zones with daylight saving (a server sets a fixed offset for now) | designed, not started |

## Gaps by area (checked against the code)

### Interactions (M11): done
- Options: attachment and mentionable added to string, integer, number, boolean, user, channel and
  role.
- Autocomplete of string, integer and number options (`autocomplete` event, the response with at
  most 25 choices, `ModuleBuilder::autocomplete`).
- Context-menu commands on a user or on a message (`ContextCommand`, `user_command`,
  `message_command`), at most five of each kind.
- Subcommand groups, two levels deep (`ModuleBuilder::groups`).
- Expiring components: `CustomId::expires_in` and `Pager::expires_in`; a press after the expiry is
  refused. Taking the dead buttons off the message is not done (see the writing-modules section).

The writing guide has a section for each: [writing modules](writing-modules.md#option-types-groups-autocomplete-context-menus-expiring-buttons).

### Events we do not deliver (M12)
Voice state changes (join, leave, move, mute), threads (member changes and updates other than
archive and unarchive), invites (create, delete), scheduled events, guild updates (name,
settings), emoji and sticker changes, AutoMod action executed, poll votes, webhook and
integration updates, presence (a privileged intent, optional), and an audit log entry created.
Delivered: role changes, threads (create, archive, unarchive, delete) and cases recorded by any
module, see [writing modules](writing-modules.md#events-about-roles-threads-and-cases).

### Actions we do not have (M12)
Done: reading messages (one, history, pins), pin/unpin, crosspost, removing reactions, listing
who reacted, threads (create, edit, join, leave, add or remove a member, list members), invites
(create, list, delete), webhooks (create, list, delete, execute), voice moderation (move,
disconnect, deafen), searching and listing members, pruning, editing the guild, reordering roles
and channels, creating a direct message channel, scheduled events (list, create, edit, delete),
emoji (list, create, delete).

Still missing: stickers, editing AutoMod rules, forum posts (a thread started in a forum
channel), editing or deleting a webhook's own settings, and the audit log reason for scheduled
events (Discord accepts one; the actions do not carry it yet).

### Services (M13)
User profile (data about a user across modules, with the user's say), message templates an
administrator can edit (a small set of safe variables), an audit trail modules write to (with
a reader in the API), a restricted HTTP client (allowed hosts, timeouts, size limits, no internal
addresses), metrics and log context per module, feature switches and a kill switch per module.

### Tooling (M14)
Done: a generator of the command, permission and configuration reference from the manifests
(`docs/reference`, checked by `docs --check` in CI); a scaffold for a new module (`new-module`); hints
in the configuration schema for the dashboard's forms (`x-ui`). Still to do: tests that a wrong use
of the macro does not compile; the explanation of why a permission was granted or refused.

## What a piece must have before it counts as done
Tests that fail first and pass after, the same tests failing when the code is broken on purpose,
fmt, clippy with warnings denied, the whole test suite against a real PostgreSQL, a section in
`docs/writing-modules.md`, and an outside review of the diff.
