# Inventory: the reference bot's moderation against closure

Read on 2026-11-06 from the reference bot's source (the command files of each plugin and
`ModActions/types.ts`). A command is **here** when closure has a command or an
equivalent that does the same, **partly** when something of it is here, **missing** when nothing
is, and **left out** when it is left out on purpose (see [parity](parity.md)). The order of
building is in the [roadmap](roadmap.md#moderation-next-work-the-owners-list-and-the-rest-of-the-reference-bot-planned-not-started).

## ModActions (commands: `addcase ban case cases deletecase forceban forcemute forceunmute hidecase kick massban massmute massunban mute note unban unhidecase unmute update warn`)

| Reference | closure |
| --- | --- |
| `warn`, `note` | here: `/warn`, `/note` (templates, escalation) |
| `mute`, `unmute` | here: `/timeout`, `/untimeout`, `/vcmute`, `/vcunmute` (by Discord or by a role) |
| `forcemute`, `forceunmute` | missing: for a member who is not in the server; closure's timeout needs a member. A mute by role could be written to apply on the next join (the rejoin re-application is here) |
| `kick`, `ban`, `unban` | here: `/kick`, `/ban`, `/unban` (timed bans) |
| `forceban` (ban an id that is not a member) | here: `/ban` takes a user who is not in the server |
| `massban` | here: `/bulk start ban` (preview, run, cancel, retry) |
| `massunban`, `massmute` | here: `/bulk start unban` and `/bulk start timeout` |
| `addcase` (a case with no action) | here: `/case add` |
| `case`, `cases` | partly: `/case`, `/warnings`; no `cases` list with filters or `--expand` → P2-5 |
| `hidecase`, `unhidecase`, `deletecase` | here: `/case hide`, `/case unhide`, `/case delete` |
| `update` (change the reason of a case) | here: `/case update` |
| config: `dm_on_warn/kick/ban`, `message_on_*`, `message_channel`, `warn_message` and the like | partly: fixed texts and templates for reason and duration; the DM and channel notice texts are not settings |
| config: `alert_on_rejoin`, `alert_channel` | here |
| config: `warn_notify_enabled`, `warn_notify_threshold` | partly: escalation of warnings (kick, ban, timeout) is here; a notice at a count is not |
| config: `ban_delete_message_days` | to confirm in `/ban` |
| config: `create_cases_for_manual_actions` | here: `manual_cases` |
| config: `can_*` (note, warn, mute, kick, ban, unban, view, addcase, massunban, massban, massmute, hidecase, deletecase, act_as_other) | here as permission keys and levels; the new commands need their own keys |
| `warn` with `--mod` (act as another moderator) | missing (`can_act_as_other`) |

## Mutes (commands: `ClearBannedMutes ClearMutes ClearMutesWithoutRole Mutes`)

| Reference | closure |
| --- | --- |
| `mutes` (list of the mutes in force, with expiry and age) | here: `/case search kind:timeout state:active` |
| `clear_mutes`, `clear_banned_mutes`, `clear_mutes_without_role` | missing |
| mute role with `remove_roles_on_mute`, `restore_roles_on_mute`, mute on rejoin, `kick_from_voice_channel_on_mute`, `move_to_voice_channel_on_mute` | here: the role method and the rejoin re-application; the voice move on mute is missing |
| mute message template with `{reason}` and `{time}` | partly |

## Cases, Logs, Persist, MessageSaver, Counters

| Reference | closure |
| --- | --- |
| Cases plugin (case log channel, hidden cases, case colours, automatic actions shown) | partly: the case log is the `logs` module and the moderation log; hiding and colours missing |
| Logs | here: `logs` module |
| Persist | here: `persist` module |
| MessageSaver (`SaveMessagesToDB`, `SavePinsToDB`) | partly: the `message_history` capability keeps messages; pins are not saved; "save messages to a case" missing |
| Counters (`add`, `list`, `reset`, `reset all`, `set`, `view`) | here: `/automod counter ...` |

## Slowmode (commands: `clear disable get list set`)

| Reference | closure |
| --- | --- |
| `slowmode set/get/disable` (native) | here: `/slowmode` |
| bot-managed slowmode beyond 6 hours, `list`, `clear` | here: `/slowmode` above 21600 seconds (0 clears it); no list |

## Censor and Spam

Covered by automod rules (words, links, invites, regex, spam windows). A set of **presets** that
gives the same result as the reference's `blocked_words`, `blocked_regex`, `domain_whitelist`,
`max_messages` and the like in one setting is missing; the roadmap says a preset must not be a
second source of truth, so it is a generator of rules (written out in the guild's document), not a
runtime path.

## Utility (commands: `About Avatar BanSearch ChannelInfo Clean Context EmojiInfo Help Info InviteInfo Jumbo Level MessageInfo Nickname NicknameReset Ping ReloadGuild RoleInfo Roles Search ServerInfo SnowflakeInfo Source UserInfo Vcdisconnect Vcmove`)

| Reference | closure |
| --- | --- |
| `avatar`, `channelinfo`, `roleinfo`, `serverinfo`, `userinfo`, `vcmove`, `vcdisconnect` | here |
| `clean` (with filters: all, bots, user, has links or invites) | partly: `/clear` |
| `bansearch` | here in part: `/case search kind:ban` finds the bans of the cases; a ban made outside the bot is a case when `manual_cases` is on |
| `search` (members by name, role, join date) | missing |
| `nickname`, `nicknamereset` | here: `/nickname` (left out, it takes the nickname off) |
| `context` (jump link of a message), `messageinfo`, `snowflakeinfo`, `emojiinfo`, `inviteinfo` | `snowflakeinfo` is here as `/snowflake`; the rest is missing |
| `level` (a member's level of permission) | missing (`/permissions explain` is planned in M14) |
| `roles` (list of the roles) | missing |
| `jumbo`, `help`, `about`, `ping`, `source` | missing, low value (`help` is Discord's own command list) |
| `reload_guild` | left out: no such thing here |

## LocateUser, NameHistory, ContextMenus

| Reference | closure |
| --- | --- |
| `where` | here: `/locate` |
| `follow`, `listfollow` (a notice when a member joins a voice channel) | missing |
| `names` | here: `/names`, and `/findname` for a search across members |
| context menus: Ban, Mute, Warn, Note, Clean message, Mod menu | missing; closure has the Info menu on a user and the message and user context command support in the platform |

## Automod

See [parity](parity.md): the triggers and actions are here, apart from what is left out on
purpose there.

## Left out on purpose

`BotControl`, `GuildAccessMonitor`, `GuildConfigReloader`, `GuildInfoSaver`, `GuildMemberCache`,
`InternalPoster`, `TimeAndDate`, `UsernameSaver` (the platform keeps what it needs), `CustomEvents`
(automod does it).
