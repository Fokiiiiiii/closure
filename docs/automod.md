# The automod module

`automod` checks every message of a server against its rules, and acts on the first rule that
trips. It is the frame the rules run in: the triggers, the spam windows, the actions, the record they
leave, a dry run, and `/automod status`. The code is in
[`crates/closure-mod-automod`](../crates/closure-mod-automod/src/lib.rs); its tests are in
[`tests/`](../crates/closure-mod-automod/tests). It is off until a server turns it on.

## Before you turn it on

- **Message Content Intent.** In the developer portal, under *Bot* → *Privileged Gateway Intents*,
  switch on **Message Content**. The module asks for it at every start, and Discord refuses the
  connection without it. Without it the bot sees no text: the word, regex, link, invite, caps,
  repeat and text-based spam checks never match. Attachments, mentions and stickers, and the spam
  measures that count them, still work.
- Permissions: **Manage Messages** (to delete), **Kick Members**, **Ban Members**,
  **Moderate Members** (timeouts), **Manage Roles** (for the `role` timeout method and the
  `add_roles` and `remove_roles` actions), **Manage Nicknames** (`set_nickname`), **Manage
  Channels** (`slowmode`), and **Send Messages** in the log channel and in any `alert` or `reply`
  channel.
- The **Server Members Intent** (privileged, under *Bot* → *Privileged Gateway Intents*) is what
  the member events need: joins, leaves, role changes and join bursts. The module asks for it at
  every start; without it those rules see no events.
- The message history (`purge`) is kept for `--message-history-days` (7 by default). A `purge` that
  reaches further back finds nothing.
- Role order: the bot's highest role must be above the members it acts on, as with the
  [moderation module](moderation.md#what-the-bot-needs-from-discord).
- Who may see the state: `automod.view` (moderators by default), for `/automod status`.

## Turning it on

```yaml
modules:
  automod:
    enabled: true
    config:
      dry_run: true          # start with a dry run, see below
      rules:
        spoilers:
          triggers: [{ type: words, words: ["spoiler"] }]
          actions: [{ do: delete }, { do: alert, channel: "123456789012345678" }]
```

Start with `dry_run: true`, read the log for a while, then turn it off.

## Configuration

| Key | Default | Meaning |
| --- | --- | --- |
| `dry_run` | `false` | Act on nothing; post what would have happened to the log channel. |
| `warn_expire_days` | `30` | After how many days a `warn` action expires (1 to 365). |
| `dm_on_action` | `true` | Whether the member is messaged about a punishment. |
| `timeout_method` | `discord` | `discord`, or `role`: a role that a timer takes off again. |
| `timeout_role` | none | The role id a `role` timeout gives. Left out, Closure creates a role named `Timed out` (or uses one of that name), with no permissions, and sets it up in every channel as moderation does. |
| `remove_roles_on_mute` | `false` | With a `role` timeout: `false`, `true` (every role of the member that can be taken off), or role ids. The roles are taken off with the role and remembered. |
| `restore_roles_on_mute` | `true` | Which of the taken-off roles come back when the timeout ends: the same forms. |
| `counters` | `{}` | The counters, keyed by name, at most 10 (see [Counters](#counters)). |
| `rules` | `{}` | An object of rules keyed by id, at most 100. |

### Role timeouts

With `timeout_method: role`, a timeout gives the dedicated role of [moderation](moderation.md#timeouts-and-voice-mutes):
the configured `timeout_role`, or the one Closure creates or finds under the name `Timed out`, which moderation
uses too, so the two modules share one role. The role denies speech in every channel (see moderation), and a
channel created later gets the same. The configuration change sets the role up before the first punishment.
`remove_roles_on_mute` and `restore_roles_on_mute` work as in moderation. The bot needs **Manage Roles** and
**Manage Channels**. The same known limit applies: an allow overwrite that another role gives in a channel wins
over the deny.

### Rules

A rule is an entry of `rules`, keyed by its id: 1 to 32 characters, lowercase letters, digits, `_`
and `-`. The key is the id; a rule has no `id` field of its own.

```yaml
rules:
  links_1:                       # the key is the id
    name: "Links"                # shown in logs; defaults to the id
    enabled: true                # default true
    triggers: [ ... ]            # 1 to 20; any one that trips trips the rule
    actions: [ ... ]             # 1 to 12; run in order
    scope: { ... }               # optional, see below
    cooldown: 10m                # optional, see below
    continue_after_match: false  # optional, see below
```

**The order of the rules is the order of their ids, as text.** A JSON object does not keep the order
it was written in, so a rule that must come first has an id that sorts first (`a_links` before
`b_words`). The first rule that trips acts, and the rules after it are not looked at for that
message, unless it sets `continue_after_match`.

A rule that is not valid never runs; `/automod status` lists it with the reason (see
[Status](#status)).

### Triggers

Each trigger has a `type`. A trigger reads the message it is given; the spam triggers also read the
messages of the same member that came before. Every example is a trigger of a rule.

```yaml
# any_message: every message trips it (use it with a scope, or a cooldown)
{ type: any_message }

# words: a word in the message (or in the username)
{ type: words, words: ["spoiler", "バカ"], whole_words: false }
{ type: words, words: ["spam"], case_sensitive: false, targets: [message, username] }
{ type: words, words: ["ｓｐａｍ"], normalize: true }      # folds width and case
{ type: words, words: ["spam"], strip_markdown: true }     # sp*am is spam
```

`words` options: `case_sensitive` (default `false`), `whole_words` (default `true`: the word stands
on its own, so `ass` does not match `class`; Japanese has no spaces, so set it to `false` for
Japanese), `normalize` (default `false`: width and case are folded, see
`closure_sdk::text::normalize`), `strip_markdown` (default `false`: `*`, `_`, `~`, `|`, `` ` `` and
backslashes are dropped first), `targets` (default `[message]`; `username` reads the author's name).

```yaml
# regex: any pattern matches (linear time; a pattern that is too big is an invalid rule)
{ type: regex, patterns: ['free\s+nitro', 'discord\.gift/\w+'] }
{ type: regex, patterns: ["SPAM"], case_sensitive: true, normalize: false }
```

Patterns are at most 20, each at most 200 characters. They are compiled once, when the rule is read,
with a size limit so that a hostile pattern cannot use the memory.

```yaml
# links: any link, or links whose domain is (or is not) in the list
{ type: links, mode: any }
{ type: links, mode: allowlist, domains: ["example.com"] }   # a link outside the list trips
{ type: links, mode: blocklist, domains: ["bad.net"] }       # a link inside the list trips
{ type: links, mode: blocklist, domains: ["bad.net"], include_subdomains: false }
{ type: links, mode: any, only_real_links: false }           # a bare example.org counts too
{ type: links, mode: any, include_regex: ["/download"] }     # only URLs matching a regex are looked at
{ type: links, mode: any, exclude_regex: ["github\\.com"] } # URLs matching a regex are never looked at
```

A domain matches its subdomains, so `example.com` covers `shop.example.com`, but not
`evil-example.com`. A link is a `http://`, `https://` or `www.` address unless `only_real_links` is
off.

```yaml
# invites: an invite whose code is not allowed trips it; blocked codes always trip
{ type: invites }                                 # any invite
{ type: invites, allow_codes: ["abc123"] }        # any invite but abc123
{ type: invites, allow_codes: ["abc123"], block_codes: ["abc123"] }  # abc123 trips too
```

```yaml
# attachments: by count, by extension, by content type
{ type: attachments, min_count: 1 }                  # at least one file
{ type: attachments, max_count: 3 }                  # more than three files
{ type: attachments, blocked_extensions: ["exe", "bat"] }   # case and a leading dot do not matter
{ type: attachments, allowed_extensions: ["png", "jpg"] }   # any other extension trips
{ type: attachments, blocked_mime_types: ["image/*"] }      # image/* is a whole family
{ type: attachments, allowed_mime_types: ["image/png"] }    # a file of another, or unknown, type trips
```

`min_count` trips when the message has **at least** that many files; `max_count` trips when it has
**more than** that many. The lists check every file of the message.

```yaml
# caps: more than max_ratio of the letters are capitals, with at least min_letters letters (10)
{ type: caps, max_ratio: 0.8, min_letters: 10 }

# repeat: one character repeated more than max_run times in a row
{ type: repeat, max_run: 6 }

# stickers: more than max stickers in one message
{ type: stickers, max: 2 }
```

### Spam

The `spam` trigger counts something a member sent, across the messages of a window, and trips when
the count reaches `amount`.

```yaml
{ type: spam, measure: messages, amount: 5, within: 10s }
{ type: spam, measure: mentions, amount: 20, within: 1m, per_channel: true }
{ type: spam, measure: characters, amount: 500 }     # no window: this one message
```

| Option | Meaning |
| --- | --- |
| `measure` | What is counted: `messages` (one per message), `mentions` (users, roles and `@everyone`), `links`, `attachments`, `emojis`, `lines`, `characters`, `stickers`, or `duplicates` (the most often repeated text). |
| `amount` | The trip point: the sum of the measure over the window reaches it (1 to 10000). |
| `within` | The window (at most one hour). Without one, the single message is measured. |
| `per_channel` | Counts each channel on its own. Needs `within`. |
| `distinct_channels` | The window has to span that many channels (2 to 50). Needs `within`. |
| `ignore_identical_messages` | A text that was sent again in the window counts once. |
| `group_identical_messages` | Every counted message has to be the same text. |

A text is "the same" when its words are the same, with single spaces (`Hi  there` and `Hi there`
are the same). Empty messages are never the same as each other.

What a burst does:

- **It fires once.** The first trip takes the messages it counted out of the window, and marks the
  member as handled for the window (`within`). Two messages that trip it at the same moment still
  fire it once.
- **Its messages are deleted.** A trip deletes every message the window counted (in one bulk
  delete per channel, or single deletes), whatever the actions of the rule are.
- **Later messages join the burst.** While the member is handled, each of their messages is
  deleted, and no punishment, reply or alert runs again for them. When the window has passed, a new
  burst can fire again.
- **A dry run counts without taking.** A dry run reports every message that would have tripped, and
  does not take or mark anything.

The windows live in memory, for an hour at most, and up to 20,000 per server; a restart starts them
empty. A spam trigger counts only the messages the rule's scope lets through.

### Events

Not every trigger reads a message. These read the other events of the server; each one hits on its
own event and on no other. A member the event does not name is left out of the actions.

```yaml
# member_join: every join; with only_new, only an account made within new_threshold (1h by default)
{ type: member_join }
{ type: member_join, only_new: true, new_threshold: 1d }

# member_join_spam: members joined within a window; every member counted is punished, and a
# member who joins while the burst is handled is punished on its own
{ type: member_join_spam, amount: 10, within: 30s }

# member_leave: a member left the server
{ type: member_leave }

# role_added, role_removed: one of the roles was given (or taken away)
{ type: role_added, roles: ["333333333333333333"] }
{ type: role_removed, roles: ["333333333333333333", "444444444444444444"] }

# case: a case the moderation module (or another module) recorded; the kinds listed, or any kind when
# none is listed. A case the bot itself recorded is left out unless include_own is set
{ type: case, kinds: [warn, ban] }
{ type: case, include_own: true }

# thread_created, thread_archived, thread_unarchived, thread_deleted: a thread of the server
{ type: thread_created }

# counter: a trigger of a counter fired (or, with reverse, its reverse did), see Counters
{ type: counter, counter: strikes, trigger: high }
{ type: counter, counter: strikes, trigger: high, reverse: true }
```

A role change the bot made itself (an `add_roles` or `remove_roles` action) is left out for ten
seconds after it was made, so that a rule does not trip on its own action. The roles of a member,
and the time it joined, are looked up when a rule needs them and the event does not carry them.

Where each event is about: a join, a leave and a role change are about the member; a case is about
its target; a thread is about the parent channel, and about its owner when Discord gives one. A
thread that was deleted has no owner, and a join has no channel.

### Scope

```yaml
scope:
  channels: ["111111111111111111"]         # only these channels (empty: all)
  exclude_channels: ["222222222222222222"]
  roles: ["333333333333333333"]            # only members with one of these roles (empty: all)
  exclude_roles: ["444444444444444444"]
  include_bots: false                      # bots are left alone unless this is true
  exempt_moderators: true                  # members at the moderator level are not checked
  member_age: { min_days: 7, max_days: 30 }  # joined 7 to 30 days ago (either bound may be left out)
```

A member whose join date Discord does not give is out of scope of a `member_age` rule. The server
owner is never checked, and neither is the bot's own message.

### Cooldown and continuing

```yaml
cooldown: 10m              # after the rule fired for a member, it is skipped for that member for 10 minutes
continue_after_match: true # the rules after this one still run when it trips
```

A cooldown is per rule and per member; a skipped rule lets the next rule look at the message. It is
kept in the module's storage, so it survives a restart, and it is not started by a dry run or by a
message that only joined a burst. It is taken before the actions run, so two messages of one member
that trip the rule at once fire it once. A cooldown that is taken stays taken when an action then
fails. At most 30 days.

### Actions

| Action | What it does |
| --- | --- |
| `{ do: delete }` | Deletes the messages of the hit. A message that is gone already is not an error. |
| `{ do: purge, within: 10m }` | Deletes what the member sent in the last `within` (at most 7 days), from the history, and the messages of the hit. Looks at the newest 100 messages of the member, at most. |
| `{ do: warn, reason: "…" }` | Warns the member; the warning expires after `warn_expire_days`. |
| `{ do: timeout, duration: 1h, reason: "…" }` | Times the member out. At most 28 days with Discord's timeout. |
| `{ do: kick, reason: "…" }` | Kicks the member. |
| `{ do: ban, duration: 7d, delete_days: 1, reason: "…" }` | Bans the member; `duration` is optional (forever when left out), `delete_days` is 0 to 7. |
| `{ do: reply, text: "…", auto_delete: 30s }` | A message in the channel of the hit. It mentions nobody. `auto_delete` removes it again after that time (at most 7 days). |
| `{ do: alert, channel: "…", text: "…" }` | A message in the channel given. It mentions nobody. Without `text` it says who, which rule and what was found, with a link to the message. |
| `{ do: add_roles, roles: ["…"] }` | Gives the roles to the member. The bot's own change is left out of the role events for ten seconds. |
| `{ do: remove_roles, roles: ["…"] }` | Takes the roles away from the member, as above. |
| `{ do: set_nickname, name: "{username} (checked)" }` | Sets the member's nickname from a template (at most 100 characters of template). |
| `{ do: slowmode, duration: 30s, channels: ["…"] }` | Sets the slowmode of the channels given, or of the channel of the event when none are given. `0s` turns it off. At most six hours. The same value is not set again within ten minutes: Discord limits how often a channel is edited. |
| `{ do: add_to_counter, counter: strikes, amount: 1 }` | Adds to a counter, for the member and the channel of the event, as the counter is kept (see [Counters](#counters)). |
| `{ do: set_counter, counter: strikes, value: 0 }` | Sets a counter, as above. |

The actions run in the order given. A failing action does not stop the next one; the first error is
logged. Every punishment is recorded as its own case, as a moderator's command would record it.
When no action punished the member (only deletions, replies or alerts), the hit is recorded as an
`automod` case, so that it shows in the log.

Durations read like `30s`, `10m`, `1h 30m`, `7d`. A `purge` looks at the history: when the history
cannot be read, that action is skipped with an error in the log, and the rest still run.

### Which action works with which event

An action that has nothing to act on in the event is skipped without a word, and the others still
run. The table says what each action needs.

| Action | Needs | Works with |
| --- | --- | --- |
| `delete` | a message, or the messages of a hit | the message triggers, spam windows |
| `purge` | a member | every event with a member |
| `warn`, `timeout`, `kick`, `ban` | a member; for a join burst, each member counted | every event with a member |
| `reply` | a message | the message triggers |
| `alert` | nothing | every event (the text may name the member, the rule and what was found) |
| `add_roles`, `remove_roles`, `set_nickname` | a member | every event with a member |
| `slowmode` | a channel: its `channels`, or the channel of the event | the message triggers, thread events, counter triggers that have a channel |
| `add_to_counter`, `set_counter` | the member and the channel the counter is kept for | every event; a missing one is an error in the log |

| Trigger | Member | Channel | Message |
| --- | --- | --- | --- |
| words, regex, links, invites, attachments, caps, repeat, stickers, any_message, spam | the author | the channel | yes |
| member_join, member_join_spam, member_leave, role_added, role_removed | the member | none | none |
| case | the target | none | none |
| thread_created, thread_archived, thread_unarchived, thread_deleted | the owner, when Discord gives one | the parent channel | none |
| counter | the member the change was for, if it had one | the channel of the change | none |

### Templates

A `reason`, a reply `text` and an alert `text` are templates. `{name}` is replaced by the value of
the variable, and `{{` and `}}` are literal braces.

| Variable | Is |
| --- | --- |
| `{user}` | a mention of the member |
| `{user_id}` | the member's id |
| `{username}` | the member's name |
| `{channel}` | a mention of the channel of the message |
| `{rule}` | the name of the rule (its id when it has no name) |
| `{detail}` | what was found, in the server's language, such as `contains the word "spoiler"` |
| `{message_link}` | a link to the message |
| `{guild}` | the server's id |
| `{count}` | how many messages the hit counted (for a spam trip, the messages of the burst) |
| `{counter}`, `{trigger}` | the counter and the trigger of a `counter` trigger (empty for the other events) |

For an event without a message, `{message_link}` is empty, and `{channel}` is empty when the event
has no channel. A join burst reports each member with its own `{user}` and `{user_id}`, while
`{username}` is the name of the member whose join made the burst fire.

A template with a broken brace is an invalid rule. A reason without a template reads as the rule and
what was found: `spoilers: contains the word "spoiler"`.

### Counters

A counter is a number the rules keep, per server and, as it is defined, per member and per channel.
Rules change it with `add_to_counter` and `set_counter`; a trigger of a counter fires a rule with a
`counter` trigger. Counters are kept in the module's storage, so they survive a restart.

```yaml
counters:
  strikes:
    per_user: true              # one value per member (channel: per_channel)
    initial_value: 0            # the value before any change; 0 by default
    decay: { amount: 1, every: 1h }   # goes down by 1 every hour
    triggers:
      high: ">= 3"              # fires when the value newly is 3 or more
      clean: { condition: "= 0", reverse_condition: "> 0" }   # fires when it reaches 0; the reverse when it rises above 0 again
rules:
  plus_1:
    triggers: [{ type: words, words: ["spam"] }]
    actions: [{ do: add_to_counter, counter: strikes, amount: 1 }]
  too_many:
    triggers: [{ type: counter, counter: strikes, trigger: high }]
    actions: [{ do: timeout, duration: 1h }]
```

- A condition is a comparison with a number: `>= 5`, `> 10`, `= 0`, `!= 3`, `< 2` or `<= 7`. A
  trigger written with just a condition has the opposite of it as its reverse.
- A trigger fires when a change takes the value from not matching its condition to matching it. Its
  reverse fires when a change takes the value from matching the condition to matching the reverse.
  So a value that stays above the condition fires nothing again.
- Decay is lazy: the value is brought down when it is read or changed, not at the time it passes.
- The values are kept per member, per channel, both, or once for the server, as the counter says.
  An event that has no member (a counter for a channel of a thread, say) cannot change a counter
  kept per member: that is an error in the log.
- A rule run because of a counter trigger may change another counter and fire another trigger. Three
  levels of this are run; a trigger fired deeper is dropped and reported as an error.
- At most 10 counters, each with at most 10 triggers.

Counters are read and changed with `/automod counters`, by a member with the `automod.counters`
permission (moderators by default):

```text
/automod counters view counter:strikes user:@member
/automod counters set counter:strikes value:0 user:@member
/automod counters add counter:strikes amount:2 user:@member
/automod counters reset counter:strikes              (every scope, back to initial_value)
```

`view` shows the current value, with the decay that has passed. A change made by a command runs the
triggers it fires, as an action does, and it changes the counter in a dry run too: it is the
moderator's own action. The reply is private.

### Dry run

With `dry_run: true` nothing is done: no deletion, no punishment, no case, no cooldown, and a burst
is not taken. For each hit, one embed titled `[dry run] automod` goes to the server's log channel,
with the rule, the member, what was found, and the actions that would have run. Without a log channel
a dry run does nothing at all.

## Status

`/automod status` (privately) shows whether dry run is on, how many rules are enabled, disabled and
invalid, each rule on a line (its triggers, its actions, its cooldown), the events the enabled rules
listen to, the counters with how each is kept and its triggers, and each invalid rule with the
reason. Invalid rules never run. A counter that is not usable (a bad definition) is reported in
place of the counters; a rule that names such a counter is invalid.

A rule is invalid when its shape is wrong, a value is out of range (a window longer than an hour, a
pattern that does not compile, a template with a broken brace, a cooldown of zero), or it has no
trigger or no action. The schema refuses such a rule when it is saved; the module still lists it if
one got in some other way.

## Limits

- The order of the rules is the order of their ids (see above).
- A spam window is at most an hour, and is kept in memory: a restart starts every window empty.
- `purge` looks at the newest 100 messages of the member in the history, at most.
- Edits that only add embeds (link previews, say) are not delivered by the gateway library, so they
  are not checked.
- Japanese text has no spaces, so `whole_words` does not match a Japanese word inside a sentence.
  Set `whole_words: false` for it.
- Administrators are not known from a message. Only the moderator level counts for the exemption, so
  an administrator without a moderator role is checked like a member.
- `distinct_users`, thread parents for spam, presets, `changePerms`, `startThread` and
  `archiveThread`, `pauseInvites`, the antiraid levels and their trigger, coordinated spam, and the
  `match_visible_names` and `match_custom_status` matchers of the older automod are not in this
  version (see [the parity list](zeppelin-parity.md)).
- A slowmode is not set again to the same value within ten minutes; a different value is set at once.
- The regexes are linear in the length of the text, and a rule's patterns are compiled once when it
  is read. The rules are read again whenever the configuration changes.
