# Zeppelin parity

The owner's TypeScript bot (a Zeppelin fork, `zeppelin-private`) is the reference for what closure's
moderation modules should do: "basically the same spec". This is what that bot has, read from its
source (`backend/src/plugins/*/types.ts`, `Automod/{triggers,actions}`), and where closure stands.
It is a checklist, not a promise; the order of work is in [roadmap](roadmap.md).

## Zeppelin's automod

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

| Zeppelin | closure |
| --- | --- |
| rule record, OR triggers, action map | list of rules, one check each, ordered actions |
| `cooldown`, `allow_further_rules`, `presets`, `conditions.member_age`, `affects_bots/self` | `scope` (channels, roles, bots, moderators) only; first hit always stops |
| `match_words` | `words` (contains / whole word); no case, normalize, loose, markdown, embeds, names |
| `match_links`, `match_invites` | `links`, `invites`: any/allow/block lists; no regex, words, guild or custom-code rules |
| windowed spam triggers | `flood` (whole minutes); mentions, emoji, repeat are per message. The window library `closure_sdk::recent` exists (groups, distinct counts, take, handled keys); no rule uses it yet |
| `match_regex`, attachment type, MIME type | not yet (`attachments` counts and extensions) |
| `role_added`, `role_removed` | available as `RolesChanged` (needs the `GUILD_MEMBERS` intent) |
| `thread_create`, `thread_archive`, `thread_unarchive`, `thread_delete` | available as `ThreadCreated`, `ThreadArchived`, `ThreadUnarchived`, `ThreadDeleted` |
| `ban`, `unban`, `kick`, `warn`, `mute`, `unmute` | available as `CaseCreated` for the cases the moderation modules record (`timeout` and `untimeout` stand for mute and unmute); `note` is not recorded yet |
| `member_join`, `member_leave`, `member_join_spam`, counter and antiraid triggers | not yet |
| 19 actions | `delete`, `warn`, `timeout`, `kick`, `ban`, `notify` |
| counters | `closure_sdk::counters` (values per scope, lazy decay, triggers and reverses, compare-and-set writes); no module uses it yet. Antiraid levels: not yet |
| message history for spam, `clean`, logs | available as the `message_history` capability: messages kept for `--message-history-days` (7 by default), with edits and deletions (see [writing modules](writing-modules.md#message-history)) |
| `Logs` event log | ModLog of cases only |
| Mutes / ModActions / Warnings message templates | fixed texts; `{user}` `{moderator}` `{server}` in templates. `closure_sdk::template` has `{name}` and `{a.b}` only, with no functions (Zeppelin's `if`, `plural`, `round`... are not ported) |
| config overrides | closure-config has overrides |
