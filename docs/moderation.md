# The moderation module

`moderation` is the module that acts on members of a server: it warns, times out, kicks and bans
them, keeps notes, and records every one of these as a case. The code is in
[`crates/closure-mod-moderation`](../crates/closure-mod-moderation/src/lib.rs); its tests are in
[`tests/moderation.rs`](../crates/closure-mod-moderation/tests/moderation.rs).

## Commands

| Command | What it does | Permission key (default level) |
| --- | --- | --- |
| `/warn user [reason]` | Warns a member. The warning expires after `warn_expire_days`, and may bring an [escalation](#escalation). | `moderation.warn` (moderator) |
| `/note user text` | Notes something about a member. A note is not a warning and does not expire. | `moderation.note` (moderator) |
| `/warnings user` | Lists the warnings of a member that still count. | `moderation.cases` (moderator) |
| `/case view number` | Shows a case as the log shows it. | `moderation.cases` (moderator) |
| `/case list user [before]` | Lists the cases of a member, newest first, 10 at a time. | `moderation.cases` (moderator) |
| `/case revoke number [reason]` | Revokes a case, once. The log shows it grey. | `moderation.revoke` (moderator) |
| `/timeout user [duration] [reason]` | Times a member out. `duration` such as `1h` or `7d`, or a date and time (see [durations](#durations-as-a-date-and-time)); at most 28 days with Discord's timeout (see [timeouts and voice mutes](#timeouts-and-voice-mutes)). Without a reason, or without a duration, a [template](#templates) is picked when one is set up. | `moderation.timeout` (moderator) |
| `/untimeout user [reason]` | Lifts the timeout of a member. | `moderation.timeout` (moderator) |
| `/kick user [reason]` | Kicks a member from the server. | `moderation.kick` (moderator) |
| `/ban user [reason] [duration] [delete_days]` | Bans a member, for good or for `duration`. `delete_days` (0 to 7) is how many days of their messages are deleted. | `moderation.ban` (admin) |
| `/unban user [reason]` | Lifts the ban of a member. | `moderation.ban` (admin) |
| `/vcmute user [duration] [reason]` | Mutes a member in voice channels, for `duration` (or until a date and time) or until `/vcunmute`. | `moderation.voice` (moderator) |
| `/vcunmute user [reason]` | Lifts the voice mute of a member. | `moderation.voice` (moderator) |

A permission key can be changed per server with `permissions.keys` (see [configuration](configuration.md)).

The moderator's reason is written to Discord's audit log as `name: reason`, cut to 512 characters.

## Settings

```yaml
log_channel: 123456789012345678
modules:
  moderation:
    enabled: true
    config:
      warn_expire_days: 30   # 1 to 365; after how many days a warning expires
      dm_on_action: true     # whether the member is sent a direct message about the action
      require_reason: false  # whether a punishment needs a reason when no template gives one
      ban_delete_days: 0     # 0 to 7; the days of messages a ban deletes unless the command says otherwise
      escalation: []         # rules: what a warning brings about at a number of active warnings
      timeout_method: discord    # discord or role: how a timeout is given
      timeout_role: null         # the id of the role a timeout gives, needed for timeout_method role
      voice_mute_method: discord # discord or role: how a voice mute is given
      voice_mute_role: null      # the id of the role a voice mute gives, needed for voice_mute_method role
      templates: {}              # the templates a moderator picks from when a reason or duration is missing
```

Every key is optional; the values above are the defaults. Unknown keys and out-of-range values are
refused when the configuration is read. The `replies` key (`private` or `public`) is available as
well, to show the replies of this module to the invoking user only or to everyone in the channel.

## Timeouts and voice mutes

`timeout_method` and `voice_mute_method` choose how `/timeout` and `/vcmute` give their punishment.

- **`discord`** (the default): Discord's own timeout, and Discord's server mute in voice channels.
  Discord's timeout lasts 28 days at most. A voice mute needs the member to be connected to a voice
  channel: when the member is not, Discord refuses it, the command replies with that error and no
  case is recorded.
- **`role`**: the member gets the role in `timeout_role` (or `voice_mute_role`), and a timer takes it
  off when the time is up. There is no 28-day limit. The server decides what the role denies: set it to deny Send Messages, Add Reactions, Create Threads and
  Speak, in the channels or categories it should apply to. The bot's highest role must be above it.
  If the timer cannot be set, the timeout still holds and the reply says so.

A voice mute without a duration stays until `/vcunmute`. With a duration, a timer lifts it. A timer for
`discord` that finds the member not in voice tries again ten minutes later, and gives up a day after the
mute was due. A new voice mute or timeout replaces the timer of an earlier one for the same member.
A timer does not end a punishment while a newer one of the same kind still stands for the member (from this
module or another). A timed ban waits: the newer ban has its own timer, or a command lifts it. A timeout by
role and a voice mute, by role or by Discord, are not lifted early either: their timer is set again for the
end of the newest one that stands, so the last one to end lifts it. If one of them has no end, the timer does
nothing, and the role or mute stays until a command lifts it.

Known limits:

- A member who leaves and rejoins gets a standing punishment again; see [rejoining members](#rejoining-members).
- A timer always lifts its punishment with the method and role it was made with. `/untimeout` and
  `/vcunmute` use the **current** method, so after a change of `timeout_method` or `voice_mute_method`
  they may not reach a punishment made with the other method. To lift such a punishment by command,
  switch the method back and run the command again. A timeout by role made under `discord` is still
  taken off by its timer; a voice mute's timer is cancelled by `/vcunmute` under `discord`, so only
  the command under `role` lifts its role.
- A voice mute by Discord needs the member in voice, both when it is made and when it is lifted.

## Rejoining members

Leaving the server takes a role off and ends a Discord timeout, so a member who leaves and rejoins
gets what still stands again when they join:

- A timeout that stands is given again: with `role` its role, with `discord` Discord's timeout for
  the time left, at most 28 days.
- A voice mute with `role` gets its role again. A voice mute with `discord` is not given again:
  Discord refuses a server mute of a member who is not in voice.
- A case that was revoked, lifted or has run out is not given again. Bans and kicks are not repeated,
  and bots are skipped.
- No case is recorded and nothing is posted to the log or sent as a direct message for it.

This needs the **Server Members** intent, see [what the bot needs from Discord](#what-the-bot-needs-from-discord).

## Escalation

`escalation` is a list of up to 10 rules. When a `/warn` brings a member to **exactly** `warns`
active warnings (the new one included), the rule's action follows, once:

```yaml
    config:
      escalation:
        - { warns: 3, action: timeout, duration: 1h }   # the third warning: a one hour timeout
        - { warns: 5, action: ban, duration: 7d }       # the fifth: a seven day ban
```

- `action` is `timeout`, `kick` or `ban`. A `timeout` needs a `duration` of at most 28 days, such as
  `1h` or `7d`. A `kick` takes no duration. A `ban` takes an optional `duration`: without one the ban
  is for good, with one it is a timed ban, as `/ban` makes it.
- It is "exactly", not "at least": a fourth warning does not repeat the timeout of the third. When
  warnings expire or are revoked, the count can fall and reach the number again, and then the action
  runs again.
- Two rules for the same number: the first one in the list is used.
- A rule that cannot be used is skipped, and the reply says so: a timeout without a duration or with
  one longer than 28 days, or a ban with a duration that cannot be read. The warning is still recorded.
- The action is carried out as a moderator's command would carry it out. A kick or a ban messages the
  member before it, a timeout after it, as `dm_on_action` allows.
- The case of the action has the moderator who gave the warning as its moderator, and a reason that
  says it was automatic, such as `Automatic: 3 active warnings (case #12)`, in the server's language.
  The number in the reason is the case of the warning.
- If Discord refuses the action, the warning stays, no case of the action is recorded, and the reply
  says that the escalation failed.

## Templates

When `/timeout`, `/kick`, `/ban` or `/vcmute` leaves out the reason (or, for a timeout, the duration) and
the server has a usable **template** for that punishment, the bot does not act yet: it shows the
moderator a menu of the templates. The pick carries the punishment out as the command would have, and
replaces the menu with its result.

```yaml
    config:
      templates:
        timeout:
          - { id: spam-1h, label: Spam, reason: Spam in several channels, duration: 1h }
        kick:
          - { id: rude, label: Rude, reason: Rude to other members }
        ban:
          - { id: raid, label: Raid, reason: Raid of the server, duration: 7d }
          - { id: scam, label: Scam, reason: Scam links }
        vcmute:
          - { id: noisy, label: Noisy, reason: Shouting in voice channels, duration: 1h }
```

Each list holds up to 25 templates. `id` is 1 to 32 characters of `a-z`, `0-9`, `_` and `-`, and it
is unique within its list. `label` is 1 to 100 characters, `reason` 1 to 512. `duration` is optional,
such as `1h` or `7d`.

- **Values in the command win.** A template's reason fills in a missing reason, and its duration fills
  in a missing duration. A timeout with a reason and no duration takes the duration of the template; a
  ban with a duration keeps it.
- **A timeout needs a duration**, from the command or from its template. A ban or a voice mute without
  one lasts for good, or until `/vcunmute`.
- **No usable template.** The punishment runs as the command gave it, without a reason if none was given.
  A timeout without a duration is refused with "Give a duration." If `require_reason` is set, a
  punishment without a reason is refused with "A reason is required." Nothing is done in either case.
- **Templates take relative durations only** (such as `1h`). A date in the saved settings would be stale
  when it is picked, so the template `duration` and the escalation rules do not accept dates.
- **Only the moderator who ran the command may pick**, and the pick checks the permission of the
  punishment again. A pick waits for ten minutes and takes an entry of the module's storage while it
  waits. A second pick, or one after the ten minutes, says that the selection expired; a template that
  was removed from the settings in the meantime says that it is gone. Neither does anything, and the
  command has to be run again.
- The menu shows each template's label, and under it its reason and duration, cut to fit Discord's
  100 characters.

Templates that cannot be used are not offered, and they do not refuse the rest of the settings:

- a `duration` that cannot be read;
- a timeout without a duration, or with one above 28 days while `timeout_method` is `discord`;
- an `id` that an earlier template of the same list already has. The earlier one is used, even when it
  is skipped itself.

A kick takes no duration, so a duration on a kick template is not used.

- **Placeholders.** A template's `reason` may contain `{user}` (the member's mention), `{moderator}`
  (the moderator's name) and `{server}` (the server's name; "this server" when Discord cannot say). They
  are filled in when the pick is made, and the case, the direct message and the log keep the filled-in
  reason, cut to 512 characters. The server's name is asked for only when the reason names it. Other
  words in braces stay as typed, and a reason the moderator typed is never filled in. The menu shows the
  reason as written.

## Undo and the history line

The reply of `/timeout`, `/ban`, `/vcmute` and `/warn`, and of a template pick for the first three, carries an
**Undo** button (**Revoke** for a warning). Pressing it does what the matching un-command does, and the
message is replaced by the result with a line that says who undid it: `/untimeout`, `/unban`,
`/vcunmute`, or, for a warning, `/case revoke`. `/kick` and `/note` have nothing to undo, so they have no
button.

- Any moderator may press the button, but each press checks the permission of the punishment again
  (`moderation.timeout`, `moderation.ban`, `moderation.voice`, or `moderation.revoke` for a warning) and
  whether the bot may act on the member.
- A press changes nothing when the case no longer stands: it was revoked, lifted by a command, or has
  run out. The presser is then told that it is already over. Buttons do not expire by themselves, so
  this check is what keeps an old button harmless.
- Undoing a timeout, a ban or a voice mute records the un-case with the reason `Undone from case #N`, and
  revokes case N, the one the button belongs to. Undoing a warning revokes it without any action on
  Discord.
- If a newer timeout, ban or voice mute of the member still stands, the button of an older one only
  revokes its own case, with the reason `Undone with its button.`, and the reply says that a newer one
  still applies. Discord and the timers are left alone, since the newer punishment is still in force;
  its own button or command lifts it.

The reply of `/timeout`, `/kick`, `/ban` and `/vcmute` ends with a history line in small text: the
active warnings and the cases the member had before this action. The new case is not counted. Up to
100 cases are counted; more are shown as `100+ cases`. The line is left out when the cases cannot be
read, and the action is not affected.

## Cases and the log

Every action is a **case**. Cases are numbered per server, and the numbers are shared with the other
modules, so a warning and a timeout from two modules are numbered together. A case has a kind
(`warn`, `note`, `timeout`, `untimeout`, `kick`, `ban`, `unban`, `vcmute`, `vcunmute`), the member,
the moderator, the reason and, for a warning or a timed action, an expiry.

- A warning counts until it is revoked or its expiry is due. Nothing has to run to expire one.
- `/untimeout` revokes the newest timeout that still stands; `/unban` revokes the newest ban that
  still stands; `/vcunmute` revokes the newest voice mute that still stands. The revoked case says
  which case did it.
- A timed ban, a timeout by role and a timed voice mute are lifted by a timer when their time is up.
  `/unban` cancels the timer of a ban, `/vcunmute` that of a voice mute.
- If an action on Discord fails, no case is recorded and nothing is posted to the log.
- Each case is posted to `log_channel`, if the server has one, in the server's language. A log that
  cannot take a post does not undo the action.

## Direct messages

When `dm_on_action` is on, the member is messaged about a warning, a timeout, a voice mute, a kick or a
ban. The message is in the server's language and gives the reason.

- A kick or a ban is messaged **before** it happens, because a kicked member shares no server with
  the bot and cannot be messaged afterwards. If the kick or ban then fails, the message was still sent.
- A timeout and a voice mute are messaged after it, since the member stays in the server.
- A member who has closed direct messages, or whom Discord does not let the bot message, does not
  fail the action. The reply says that the message could not be delivered. A member who is not in
  the server gets no note: Discord answers that it cannot find the member.
- Discord limits how many direct messages a bot may send, so a burst of actions can miss some.
- The duration in a message is worded in the language of the command, which the SDK chooses. The
  direct message shows the length only; the reply of `/timeout`, `/ban` and `/vcmute` also shows the end as
  a Discord timestamp when a date was given.

## Durations as a date and time

`/timeout`, `/ban` and `/vcmute` take a length (`1h30m`, `7d`) or a moment. The forms are read without
regard to letter case, and extra spaces are allowed:

| Form | Examples | Reads as |
| --- | --- | --- |
| date and time | `2026-11-01 18:00`, `2026/11/01 18:00:30`, `2026-11-01T18:00` | that moment; seconds are optional |
| date only | `2026-11-01` | 00:00 of that day |
| Japanese date | `2026年11月1日 18時30分`, `2026年11月1日18時` | that moment |
| month and day | `11/01 18:00`, `11月1日 18:00` | the next such moment, in this or the next year |
| time only | `18:00`, `9:05`, `18時`, `18時30分` | the next such time: today if still ahead, else tomorrow |
| words | `today 18:00`, `tomorrow 18:00`, `今日 18:00`, `明日 18:00`, `明後日 9時` | that day at that time |
| Discord timestamp | `<t:1767225600>`, `<t:1767225600:R>` | that moment, pasted from a message |

A zone may end the text: `Z`, `UTC`, `JST`, `+09:00`, `+0900`, `+9` or `-05:00`. Without one, the
zone is the server's `timezone` setting when it has one (see [time zone, prefix and emojis](configuration.md#time-zone-prefix-and-emojis)).
Otherwise it is chosen by the **language of the server**: Japanese is UTC+09:00, every other language is
UTC. The server's zone is a fixed offset, so a zone with daylight saving time has to be changed by hand.

A moment must be in the future and no more than ten years away. Years are 2000 to 2200, and the day must
exist (no 2026-02-30, no 2100-02-29). Hours run from 0 to 23, minutes and seconds from 0 to 59. A moment
that has passed is refused before anything is done, and so is a text that is no time: the reply says
"Unknown time" and gives an example. A timeout that Discord refuses (more than 28 days) keeps its message
even when the end is a date.

```text
/timeout user 2026-11-01 18:00 spam
/ban user raid 2026-12-24 09:00 JST
/vcmute user 18:00 loud
```

## Timed bans

A ban with a duration sets a timer in the bot's scheduler. When the time is up, the ban is lifted
and no case is posted for it. If the timer cannot be set, the ban still holds and the reply says so;
lift it with `/unban` or ban it again with a duration.

## What the bot needs from Discord

- Permissions: **Kick Members**, **Ban Members**, **Moderate Members** (for timeouts), **Mute Members**
  (for voice mutes), **Manage Roles** (for the `role` methods), and **Send Messages** in the log
  channel. The module does not need View Audit Log.
- Gateway intent: **Server Members**, a privileged intent for the [rejoining members](#rejoining-members).
  Switch it on for the bot under *Bot* → *Privileged Gateway Intents* in the developer portal. The
  bot asks Discord for it at every start, so without it Discord refuses the connection.
- Role order: the bot's highest role must be above the highest role of the member it acts on, as
  Discord requires, and above the role that a `role` method gives. The bot refuses, with a message, to act on the server owner, on a bot, on itself,
  on the moderator who ran the command, or on someone whose highest role is not below the moderator's.
- Direct messages: members receive a bot's direct messages only from servers they share with it,
  and only if their privacy settings allow it.
