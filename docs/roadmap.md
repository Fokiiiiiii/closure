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
| — | **Automod, parts 1 and 2** (done): message triggers and spam windows, the member, role, thread and case events, counters with `/automod counter`, actions (delete, purge, warn, timeout, kick, ban, reply, alert, roles, nicknames, slowmode, counters), cooldown, `continue_after_match`, scope with member age, templates, dry run and `/automod status` (see [automod](automod.md) and [parity list](parity.md)). Antiraid levels are part 3 (below) | the rules are the server's; the frame stays |
| — | **Automod, part 3: antiraid** (done): named levels with per-level slowmode (restored when a level goes down), invite pause (only when antiraid paused them), quarantine of joining members (never released automatically: `/automod antiraid release` and `release-all`), timed levels, `/automod antiraid view\|set\|clear\|release\|release-all`, the `antiraid_level` trigger and the `set_antiraid_level` action (see [automod](automod.md#antiraid)). Still to do: coordinated spam, and a quarantine that holds more than 5,000 members | raids are stopped by the server's rules and its levels |
| M10 | Message kit: richer `MessageInfo`, text matching helpers, the content intent: **done** (see [writing modules](writing-modules.md#reading-messages)). Message history: **done** (see [writing modules](writing-modules.md#message-history)). Still to do: optional bounded cache | automod and logging |
| M11 | Interaction surface: **done** (below; the clearing of expired buttons is not) | convenience for every module |
| M12 | Events and actions that Discord has and we do not (below) | breadth |
| M13 | Services: user profile, templates, audit trail for modules, HTTP client, metrics, feature switches | breadth |
| M14 | Tooling: compile-fail tests for the macro, button expiry, `/permissions explain`; done: docs generator (`cargo run -p closure-xtask -- docs`), module scaffold (`new-module`), x-ui hints | quality |
| — | Simple and advanced views of the configuration: the API is done (`view=basic` on the schema, the document and validation; see [configuration](configuration.md#simple-and-advanced-views)); the dashboard form that uses it comes later | a route between easy and advanced settings |
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

- Dedicated roles for the role method: `Mute` and `VC mute` (named in the server's language, red) created on first use with no permissions, set up in every channel by a sync job, with `remove_roles_on_mute` and `restore_roles_on_mute` taking roles off and giving them back. Known limit: another role's allow overwrite in a channel wins over the deny (counted in the sync note).

## What a piece must have before it counts as done
Tests that fail first and pass after, the same tests failing when the code is broken on purpose,
fmt, clippy with warnings denied, the whole test suite against a real PostgreSQL, a section in
`docs/writing-modules.md`, and an outside review of the diff.

## Moderation, next work: the owner's list and the rest of the reference bot (planned, not started)

Added 2026-11-06. This is a plan only: nothing below is built yet. The rule of the platform
comes first still holds, and so does the target the owner set: **servers of hundreds of thousands
of members**. Every piece below is written for that size: what grows with the members (cases,
names, warnings, quarantine, tickets) is kept in the module scores and sets or in a table of its
own, never in the key-value entries (10,000 a module and guild); a job does one small piece of
work and can be resumed; a write costs the same in a server of a hundred members and of a
million. Every piece also has to pass the audit of 2026-11-06 (see
`project_closure_audit_2026_11_06` in the owner's notes): permissions checked at every way in
(slash, prefix, button, reaction, dashboard), a side effect only after its intent is saved, and a
result nobody can lose.

### P1

| # | What | Why and how | Platform pieces it needs |
| --- | --- | --- | --- |
| P1-1 | **Cases for punishments made by hand** (done) | A ban, kick or timeout a person makes in Discord itself (the member list, a context menu, another bot) becomes a case, with the moderator the audit log names. The reference bot has `create_cases_for_manual_actions` and recognises a timeout set by hand. Includes the lift: an unban or a timeout removed by hand ends the case. Cases of this origin carry the source `manual` (see P2-8). A punishment the bot made itself is not counted twice (match by the audit log entry and the intent the module saved). | the audit-log entry event (M12, "an audit log entry created") or the ban, kick and timeout events with a read of the audit log; `ReadAuditLog` capability (exists); a dedupe key on cases |
| P1-2 | **Staff alert when a member with a history comes back** (done) | `alert_on_rejoin`: on a join, if the member has warnings, notes or punishments of the last N days, post the history (count, latest cases, who) in the staff channel. A threshold (`min_cases`, `within`), the channel, the roles to mention, quiet for members without a history. Reads the cases of the member through `Cases::standing` and `for_target`, not a scan. Pairs with the existing rejoin re-application of punishments. | member joined event (exists), `Cases` (exists); a staff channel setting in `moderation` |
| P1-3 | **Bulk punishment: preview, run, history** | `massban`, `massmute`, `masskick` and the like show first: who is hit, who cannot be (permission, role below, owner, bot, already banned), and how many calls it takes (with Discord's rate limit). Then a confirm button, then a run as small resumable jobs (a batch each), with progress, a cancel, a retry of the failures, and a result list (done, skipped with the reason, failed with the error). The run is a record of its own (an operation id): it survives a restart and shows in the dashboard. Replaces the present `massban` that does up to 50 members in one handler (audit M19). | jobs with a cancel that is atomic with the start (audit M12), an operation table (scores and sets are not enough: a row per run and per target), buttons and pages (exist), `ApiClient` rate limits |

### P2

| # | What | Why and how |
| --- | --- | --- |
| P2-1 | **Search of names and nicknames** | `/names` for one member exists (the names a member went by). Added: find members by a name they used (`/names find <text>`), across accounts and nicknames, with the date; a name index in a table, not in per-member entries, with a retention (default one year) and the member's right to have it removed. |
| P2-2 | **Softban** | Ban with the messages of the last 1 to 7 days deleted, then unban at once. One command, one case of kind `softban`, kept as an intent so a failure between the two steps is finished or undone and never leaves a ban behind. In the reference of Red; the owner wants it. |
| P2-3 | **Voice management** | Voice kick and move exist (`/vcmove`, `/vckick` in `utility`). Added: move many (a whole channel to another), a **move-spam** trigger for automod (a member who moves between channels more than N times within a time: message, timeout, voice mute), and the move log. Needs the voice state event (exists) and counters. The reference bot has voice-move watching and spam settings. |
| P2-4 | **Long slowmode** | Discord stops at 6 hours. Added: a slowmode kept by the bot for any length (days), per channel, with a bypass for roles, and a choice between Discord's own (up to 6 h) and the bot's (above). The bot's version deletes a message that comes too soon and tells the member once (an ephemeral-like short notice), counts in a score per channel and member, and is off the hot path for servers that do not use it. Pairs with the reference bot's two modes. |
| P2-5 | **Case search** | Not only by target: by moderator, kind, period, source (command, automod, manual, dashboard, script), state (active, expired, revoked, appealed), and text in the reason; paged, in the dashboard and in a command; **CSV export** (a job that writes a file and sends it, for large results). Needs indexes on the cases table (moderator, kind, created, state) and a bounded query. |
| P2-6 | **Appeals** | An appeal of a ban or timeout is a record linked to the case: who appealed (the member, through a form in the dashboard or a button in the DM and the staff channel), the evidence, who reviews, the decision (accepted, rejected, needs more), and the reason; an accepted appeal can lift the punishment in the same step (through the same state machine as an unban). The member sees the state. Needs a cases link table and a small public form with a rate limit. |
| P2-7 | **Approval of heavy punishments** | A permanent ban, a ban of many, or a ban by a moderator under a chosen level is not done at once: it waits for a second member with the right level (not the same person), through a button, with a time limit. Settings: which actions, which levels need approval, how many approvers, the expiry. Built on the operation records of P1-3, so a pending approval survives a restart. |
| P2-8 | **Case source and the filters that need it** (the source is done; the filters come with P2-5) | Every case records where it came from (`command`, `button`, `automod`, `manual`, `dashboard`, `script`, `appeal`). A small change of the case record and of `Cases::create`; P1-1, P2-5 and the audit trail use it. |

### Tickets (a module in a repository of its own)

A **ticket module** for support, appeals and reports, in the plugin repository
(`closure-plugins`, one more crate `plugin-tickets` and one line in `closure-plugins`), not in
closure itself. If the owner wants a repository of its own for it, the crate is made the same
way and listed through the same aggregate crate; the module code does not change.

- A **panel**: a message with a button (or several, one per category: support, appeal, report,
  application); the buttons are the owner's, with a label, an emoji and a staff role for each.
- A **ticket** is a private channel (or a private thread) made on a press, named after the
  member and a number, with the member and the staff roles of the category able to see it, in a
  category channel; at most N open tickets for a member (a score per member), a cooldown.
- A **form** (modal) at opening: a subject and a few questions the category defines, posted at
  the head of the ticket.
- **Staff tools**: claim and release, add and remove a member, rename, set a priority, a note
  only the staff sees, a ping of the claim holder after a silence.
- **Closing**: a button or `/ticket close <reason>`; an optional confirmation; the member may
  close their own. At closing the log of the ticket (**a transcript**, text or HTML, built from
  the message history, sent as a file to a log channel and to the member by DM), then the channel
  is deleted after a delay, or archived.
- **Auto close** after N days without a message, with a warning first (a job).
- **Cases**: a ticket of the kind appeal is linked to the case it concerns (P2-6), and a report
  can start a case.
- **Scale**: the open tickets of a member and the counts are scores and sets; the transcript is
  read from the history by pages and sent as a file, never held whole; creating the channels
  respects Discord's limit (500 channels, 50 in a category: a category overflows to the next).
- **Dashboard**: the panel and the categories are edited in the dashboard (a list editor for the
  categories, the existing message composer for the panel).
- Needs: channel creation with overwrites (exists), threads (exist), file send (exists since
  `Action::PostMessage` has files), message history and the `keep_attachments` capability
  (exist), modals (exist), jobs (exist). No new platform piece.

### The rest of the reference bot's moderation (to bring over)

The owner asked for **all** of the reference bot's moderation to be here. What is already here is
in the parity list above; the table is what is not, to be checked one by one against the
reference bot's source (`backend/src/plugins/{ModActions,Mutes,Cases,Slowmode,Censor,Spam,Utility,LocateUser,NameHistory,MessageSaver,Phisherman,ContextMenus}` and the automod triggers and actions) before each is built. The first step of
this work is that inventory, which is a file listing every command and setting of those plugins
with closure's state beside it (done, partly, missing, left out on purpose).

| Reference plugin | Not here yet (to confirm against its source) |
| --- | --- |
| ModActions | forceban (ban a user who is not a member, by id), massunban, massmute, `addcase` (a case with no punishment), `update` (change the reason of a case and who made it), `hidecase` / `unhidecase` / `deletecase`, the notes and warnings views (`notes`, `warns`), `cases --expand`, the DM and channel notice templates for each action, `ban_delete_message_days`, `warn_notify_threshold` and the notify channel, the checks the reference has before acting (`is_not_self`, hierarchy) |
| Mutes | the mute list (`mutes`, with expiry and age), `forcemute` / `forceunmute` for a member who is not in the server, `clear_mutes_without_role`, mute reapplied on rejoin (done in the rejoin re-application), a message to the muted member with `{reason}` and `{time}`, `kick_from_voice_channel_on_mute`, `move_to_voice_channel_on_mute` |
| Cases | `case_log_channel` per kind, `show_antiraid_and_automod_cases`, case hiding, the case list with filters (P2-5), `log_automatic_actions`, `case_colors` |
| Slowmode | `slowmode_list`, `slowmode_clear`, bot-managed slowmode (P2-4), `use_native_slowmode`, per-role bypass |
| Censor | a separate word, invite and domain censor with `blocked_tokens`, `blocked_words`, `blocked_regex`, `domain_whitelist` (closure does this through automod rules: a **preset** of rules that gives the same in one setting is the work, without a second source of truth) |
| Spam | the older `max_messages`, `max_mentions`, ... limits (covered by automod spam triggers: a preset, as above) |
| Utility | `clean` variants (`clean all`, `clean bot`, `clean user`, `clean --has-invites`), `search` for members by name, role, join date, `banSearch`, `nickname`, `level` of a member, `jumbo`, `lock` / `unlock` of a channel (a `channel_controls` piece exists), `info` extras (`messages`, `cases` counts) |
| LocateUser | `where` (done: `/locate`), `follow` (a notice to the moderator when the member joins a voice channel) |
| NameHistory | `names` (done), search across members (P2-1) |
| MessageSaver | a message kept to a case (`save-messages-to-case`: attach the messages the history has around a deletion to a case, as a file) |
| Phisherman | done through the phishing module (the outside API) |
| ContextMenus | the user context menu for moderators: Warn, Mute, Kick, Ban, Note, Cases, Clean (the message and user context commands exist; the moderation ones do not) |
| Counters / CustomEvents | counters are here; custom events are covered by automod triggers and actions (not planned as a separate module) |

### Order of this work

1. **Inventory** of the reference moderation plugins against closure (a file in `docs/`, no code).
2. **P2-8** case source (a small change the rest uses), then **P1-1** manual punishments, then **P1-2** the rejoin alert, which are both small once the source is there.
3. The **operation record** and its jobs (the base of P1-3, P2-7 and the state machine of the audit's H05), then **P1-3** bulk punishment, then **P2-7** approval.
4. **P2-2** softban, **P2-5** case search and export, **P2-6** appeals.
5. The missing ModActions, Mutes, Cases and Utility commands of the table above, with the context-menu commands.
6. **P2-1** name search, **P2-3** voice, **P2-4** long slowmode.
7. **Tickets** in the plugin repository, once the operation record exists (a ticket closing is one).

Everything above comes after the open items of the audit of 2026-11-06 that touch the same code
(the punishment state machine, the jobs and outbox, the quarantine table): those are the ground the
new moderation stands on.
