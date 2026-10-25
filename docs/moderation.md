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
| `/timeout user duration [reason]` | Times a member out. `duration` such as `1h` or `7d`; at most 28 days with Discord's timeout (see [timeouts and voice mutes](#timeouts-and-voice-mutes)). | `moderation.timeout` (moderator) |
| `/untimeout user [reason]` | Lifts the timeout of a member. | `moderation.timeout` (moderator) |
| `/kick user [reason]` | Kicks a member from the server. | `moderation.kick` (moderator) |
| `/ban user [reason] [duration] [delete_days]` | Bans a member, for good or for `duration`. `delete_days` (0 to 7) is how many days of their messages are deleted. | `moderation.ban` (admin) |
| `/unban user [reason]` | Lifts the ban of a member. | `moderation.ban` (admin) |
| `/vcmute user [duration] [reason]` | Mutes a member in voice channels, for `duration` or until `/vcunmute`. | `moderation.voice` (moderator) |
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
      require_reason: false  # whether warn, timeout, kick, ban and vcmute need a reason
      ban_delete_days: 0     # 0 to 7; the days of messages a ban deletes unless the command says otherwise
      escalation: []         # rules: what a warning brings about at a number of active warnings
      timeout_method: discord    # discord or role: how a timeout is given
      timeout_role: null         # the id of the role a timeout gives, needed for timeout_method role
      voice_mute_method: discord # discord or role: how a voice mute is given
      voice_mute_role: null      # the id of the role a voice mute gives, needed for voice_mute_method role
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

Known limits:

- With `role`, a member who leaves the server loses the role, so a member who rejoins is not muted or
  timed out any more.
- A timer always lifts its punishment with the method and role it was made with. `/untimeout` and
  `/vcunmute` use the **current** method, so after a change of `timeout_method` or `voice_mute_method`
  they may not reach a punishment made with the other method. To lift such a punishment by command,
  switch the method back and run the command again. A timeout by role made under `discord` is still
  taken off by its timer; a voice mute's timer is cancelled by `/vcunmute` under `discord`, so only
  the command under `role` lifts its role.
- A voice mute by Discord needs the member in voice, both when it is made and when it is lifted.

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
- The duration in a message is worded in the language of the command, which the SDK chooses.

## Timed bans

A ban with a duration sets a timer in the bot's scheduler. When the time is up, the ban is lifted
and no case is posted for it. If the timer cannot be set, the ban still holds and the reply says so;
lift it with `/unban` or ban it again with a duration.

## What the bot needs from Discord

- Permissions: **Kick Members**, **Ban Members**, **Moderate Members** (for timeouts), **Mute Members**
  (for voice mutes), **Manage Roles** (for the `role` methods), and **Send Messages** in the log
  channel. The module does not need View Audit Log.
- Role order: the bot's highest role must be above the highest role of the member it acts on, as
  Discord requires, and above the role that a `role` method gives. The bot refuses, with a message, to act on the server owner, on a bot, on itself,
  on the moderator who ran the command, or on someone whose highest role is not below the moderator's.
- Direct messages: members receive a bot's direct messages only from servers they share with it,
  and only if their privacy settings allow it.
