# Parity with the reference bot

The owner's TypeScript bot is the reference for what closure's
moderation modules should do: "basically the same spec". This is what that bot has, read from its
source (`backend/src/plugins/*/types.ts`, `Automod/{triggers,actions}`), and where closure stands.
It is a checklist, not a promise; the order of work is in [roadmap](roadmap.md).

## The reference bot's automod

A rule is an entry of a record (`rules: { my_filter: {...} }`) with:
`enabled`, `pretty_name`, `presets`, `affects_bots`, `affects_self`, `conditions.member_age`
(min/max days), `cooldown`, `allow_further_rules` (default: the first matching rule stops the
rest), `triggers` (a list; any one matching trips the rule) and `actions` (a map; all run).

Triggers (38): messages — `any_message`, `match_words`, `match_regex`, `match_invites`,
`match_links`, `match_attachment_type`, `match_mime_type`, `has_attachments`; windowed spam
(`amount` within `within`, plus `per_channel`, `distinct_channels`, `distinct_users`,
`ignore_identical_messages`, `group_identical_messages`, `threads_as_parent_channel`) —
`message_spam`, `mention_spam`, `link_spam`, `attachment_spam`, `emoji_spam`, `line_spam`,
`character_spam`, `sticker_spam`, `thread_create_spam`, `coordinated_message_spam`,
`coordinated_link_spam`; members — `member_join` (only_new/new_threshold), `member_join_spam`,
`member_leave`, `role_added`, `role_removed`; moderation — `ban`, `unban`, `kick`, `mute`,
`unmute`, `warn`, `note`; threads — `thread_create`, `thread_archive`, `thread_unarchive`,
`thread_delete`; `counter_trigger`, `antiraid_level`.

Text matchers share: `case_sensitive`, `normalize`, `strip_markdown`, `loose_matching`(+threshold),
`only_full_words`, and where to look: `match_messages`, `match_embeds`, `match_visible_names`,
`match_usernames`, `match_nicknames`, `match_custom_status`. Links: include/exclude domains,
subdomains, words, regex, `only_real_links`. Invites: include/exclude guilds, invite codes,
custom codes, group DM invites.

Actions (19): `clean`, `warn`, `mute`, `kick`, `ban` (each: `reason`, `duration`, `notify`
(dm/channel), `notifyChannel`, `postInCaseLog`, `hide_case`; mute: `remove_roles_on_mute`,
`restore_roles_on_mute`; ban: `deleteMessageDays`), `alert` (text to a channel, allowed mentions),
`log`, `reply` (text or embed, `auto_delete`, `inline`), `addRoles`, `removeRoles`,
`changeNickname`, `changePerms`, `addToCounter`, `setCounter`, `setSlowmode`, `startThread`,
`archiveThread`, `pauseInvites`, `setAntiraidLevel`.

Around it: per-server `antiraid_levels` (none/low/medium/high/lockdown) with slowmode, invite pause
and quarantine; `Counters`; `Spam` (older max_* limits), `Censor` (older word/domain/invite
filter), `Mutes` (mute role, voice move/kick, message templates with `{guildName}`, `{reason}`,
`{time}`), `ModActions` (DM and channel notices with templates), `Warnings` (thresholds),
`Cases`, `Logs` (event log of edits, deletes, joins...), `MessageSaver` (every message kept, which
spam detection, `clean` and logs rely on), config overrides by level/channel/role.

## Where closure stands

| Reference bot | closure |
| --- | --- |
| rule record, OR triggers, action map | list of rules, one check each, ordered actions |
| `cooldown`, `allow_further_rules`, `conditions.member_age`, `affects_bots/self` | `cooldown` (per rule and member), `continue_after_match`, `scope` (channels, roles, bots, moderators, `member_age`). No `presets` |
| `match_words` | `words`: whole word or contains, case, `normalize`, `strip_markdown`, message or username targets. No loose matching, no embeds |
| `match_links`, `match_invites` | `links`: any, allowlist, blocklist, subdomains, bare domains, include and exclude regexes. `invites`: allowed and blocked codes. No guild rules |
| windowed spam triggers | `spam`: messages, mentions, links, attachments, emojis, lines, characters, stickers, duplicates; `within`, `per_channel`, `distinct_channels`, identical-text options, burst handled once. No `distinct_users`, no thread-parent channels |
| `match_regex`, attachment type, MIME type | `regex` (linear time, size-limited), `attachments` (count, extensions, MIME types with `type/*`) |
| `role_added`, `role_removed` | `role_added`, `role_removed`: a role given or taken away (from `RolesChanged`; the bot's own role changes are left out for ten seconds) |
| `thread_create`, `thread_archive`, `thread_unarchive`, `thread_delete` | `thread_created`, `thread_archived`, `thread_unarchived`, `thread_deleted`. No `thread_create_spam` yet |
| `ban`, `unban`, `kick`, `warn`, `mute`, `unmute` | `case` with `kinds`: the cases the moderation modules record (`timeout` and `untimeout` stand for mute and unmute); `note` is not recorded yet |
| `member_join` (`only_new`, `new_threshold`), `member_leave`, `member_join_spam` | `member_join`, `member_join_spam`, `member_leave` |
| `counter_trigger` | `counter` (a trigger of a counter fired, or its reverse) |
| `antiraid_level` | `antiraid_level` (the level changed, to or from the levels named). Its context carries `{level}` and `{old}` |
| 19 actions | `delete`, `purge`, `warn`, `timeout`, `kick`, `ban`, `reply` (with `auto_delete`), `alert`, `addRoles` (`add_roles`), `removeRoles` (`remove_roles`), `changeNickname` (`set_nickname`), `setSlowmode` (`slowmode`), `addToCounter` (`add_to_counter`), `setCounter` (`set_counter`), `setAntiraidLevel` (`set_antiraid_level`). Templates with `{user}`, `{rule}`, `{detail}` and more |
| antiraid levels: `antiraid_levels` (none, low, medium, high, lockdown) with their slowmode, invite pause and quarantine | the module's `antiraid` section: named levels (the first is calm), per-level `slowmode` (restored when the level goes down), `pause_invites` (only when antiraid paused them), `quarantine` (new members of that level; the role is created as `Quarantine` when none is given, and it is never released automatically: `/automod antiraid release` or `release-all`), `notify_channel`; `/automod antiraid view\|set\|clear\|release\|release-all`. A timed level goes back to calm |
| counters | the module's `counters`: per member, per channel or overall, lazy decay, triggers and reverses, changed by actions and by `/automod counters`. The reference bot's counter commands are `/automod counters view\|set\|add\|reset` here |
| message history for spam, `clean`, logs | available as the `message_history` capability: messages kept for `--message-history-days` (7 by default), with edits and deletions (see [writing modules](writing-modules.md#message-history)) |
| `Logs` event log | the `logs` module: edits, deletions (with the text and attachments the history kept), joins, leaves and bans, to a channel. The old text of an edited message is not kept. Files are fetched when a deletion or an edit needs them, and kept as they arrive with `keep_attachments` (see [logs](logs.md)) |
| Mutes / ModActions / Warnings message templates | fixed texts; `{user}` `{moderator}` `{server}` in templates. `closure_sdk::template` has `{name}` and `{a.b}` only, with no functions (the reference bot's `if`, `plural`, `round`... are not ported) |
| config overrides | closure-config has overrides |
| text commands with a prefix (`!warn @user spam`) | `prefix` per guild; the words fill the options of the slash command of the same name, which runs with the same handler and checks (see [text commands](writing-modules.md#text-commands)) |

## Order of the missing features

The order is chosen by three things: whether the platform already has the pieces (events, jobs,
buttons, storage), how much a moderation bot needs the feature, and the roadmap's rule that the
platform comes first. Events that closure does not deliver yet (voice state, user updates, invites,
guild and emoji updates) block the features that need them; those wait for M12 in the
[roadmap](roadmap.md).

**P1: the platform has everything; do these first**

1. Logs (done, the `logs` module): an event log of edits, deletions, joins, leaves and bans, posted to a channel. The events
   exist (message updated and deleted, member joined and left, bans). The largest gap for moderation.
2. WelcomeMessage: a message on join, with templates.
3. A Slowmode command for moderators (the action exists).
4. AutoReactions and AutoDelete: a reaction on every message in a channel, or deletion after N seconds.
   These use the message events and the existing actions and jobs.

**P2: a module of its own, with no new platform**

5. Tags and CommandAliases: stored text as a command, and aliases of commands.
6. Reminders and Post: scheduled messages, on the jobs system.
7. Roles: ReactionRoles, RoleButtons, SelfGrantableRoles and PingableRoles as one module. The reaction
   events and the buttons exist; the role hierarchy checks apply.
8. Starboard: reaction events and a fetch of the message.

**P3: waits for a platform piece, or for a decision**

9. NameHistory and UsernameSaver: nickname changes come from member updates, but username changes
   need the user-update event (M12).
10. LocateUser and CompanionChannels: they need the voice state events (M12).
11. Leveling: storage, message events and role rewards. Large.
12. UrlThreatDetection: needs the restricted HTTP client (M13) and an outside service.
13. GuildStats, CustomEvents and InternalPoster: the value is unclear; decide later.

**Not planned, until the owner says otherwise:** Casino and Grok, and the internal plugins
(GuildInfoSaver, GuildMemberCache, GuildConfigReloader, TemporaryGuildProfile, GuildAccessMonitor).
closure does without them.

## Deliberately not taken

These are in the reference bot and are left out on purpose, for now:

- `presets`: a rule is written out in full; a preset would be a second source of truth.
- `changePerms`: channel permission edits are a moderator's decision, not a rule's.
- `startThread` and `archiveThread`: threads are for the members, not for automod.
- `pauseInvites` as a rule action: the invites are paused by antiraid levels only (see the antiraid
  row); a rule cannot switch them off.
- Coordinated spam (`coordinated_message_spam`, `coordinated_link_spam`): it needs the messages of many
  members compared at once, and is not built yet.
- `match_visible_names` and `match_custom_status`: the gateway does not give the names shown in a
  server, or the custom statuses, to the module.
