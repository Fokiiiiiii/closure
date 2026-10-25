# Roadmap of the module platform

The rule for what goes in: **a layer that might be used is better in than out.** Modules are only
as good as the platform under them, so the platform covers Discord widely, even where no module
needs a piece yet. Every piece comes with tests that were seen failing first, and the same audit
as the rest.

State at the time of writing: the SDK has commands (derive macro, subcommands, cooldown,
permission levels), configuration in three layers, storage and counters, event subscriptions,
buttons, menus, forms and pages, jobs, durations, a check of who a moderation may act on, and
the reference module `closure-mod-moderation`.

## Order of work

| Step | What | Why first |
| --- | --- | --- |
| M9a | `closure_sdk::embed`: an embed builder that checks Discord's limits | ModLog and most modules |
| M9b | Cases: records of moderation, numbered per guild, shared by modules (host service, capability `cases`) | moderation and automod share them |
| M9c | ModLog: posts a case to the log channel in the guild's language | uses M9a and M9b |
| M9d | Direct messages to a user (`Action::SendDirectMessage`), with the refusals Discord gives | warn/ban notices |
| — | **`moderation` module**: ban, kick, timeout, warn, cases, log (warn, note, cases and the log are done; ban, kick and timeout come next) | first real module |
| M10 | Message kit: richer `MessageInfo`, message history, optional bounded cache, text matching helpers, the content intent | automod and logging |
| M11 | Interaction surface (below) | convenience for every module |
| M12 | Events and actions that Discord has and we do not (below) | breadth |
| M13 | Services: user profile, templates, audit trail for modules, HTTP client, metrics, feature switches | breadth |
| M14 | Tooling: compile-fail tests for the macro, button expiry, docs generator, module scaffold, `/permissions explain`, x-ui hints for the dashboard | quality |
| later | Workflow, module-to-module calls, hot reload (Lua/WASM) | designed, not started |

## Gaps by area (checked against the code)

### Interactions (M11)
- Options: string, integer, number, boolean, user, channel and role exist; attachment and
  mentionable are missing.
- Autocomplete of options (needs a new event and a response type).
- Context-menu commands on a user or on a message.
- Subcommand groups (two levels of nesting).
- Buttons that run out: a component made with an expiry, refused after it and cleaned up.

### Events we do not deliver (M12)
Voice state changes (join, leave, move, mute), threads (create, update, delete, members),
invites (create, delete), scheduled events, guild updates (name, settings), emoji and sticker
changes, AutoMod action executed, poll votes, webhook and integration updates, presence (a
privileged intent, optional), and an audit log entry created.

### Actions we do not have (M12)
Reading messages (one, history, pins), pin/unpin, crosspost, removing reactions, listing who
reacted, threads (create, archive, join, add or remove a member), invites (create, list, delete),
webhooks (create, send), scheduled events, voice moderation (move, disconnect, mute, deafen),
searching and listing members, pruning, editing the guild, emoji and sticker management,
reordering roles and channels, editing AutoMod rules, creating a direct message channel.

### Services (M13)
User profile (data about a user across modules, with the user's say), message templates an
administrator can edit (a small set of safe variables), an audit trail modules write to (with
a reader in the API), a restricted HTTP client (allowed hosts, timeouts, size limits, no internal
addresses), metrics and log context per module, feature switches and a kill switch per module.

### Tooling (M14)
Tests that a wrong use of the macro does not compile; a generator of the command, permission and
configuration reference from the manifests; a scaffold for a new module; the explanation of why
a permission was granted or refused; hints in the configuration schema for the dashboard's forms.

## What a piece must have before it counts as done
Tests that fail first and pass after, the same tests failing when the code is broken on purpose,
fmt, clippy with warnings denied, the whole test suite against a real PostgreSQL, a section in
`docs/writing-modules.md`, and an outside review of the diff.
