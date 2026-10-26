# The automod module

`automod` checks every message of a server against its rules, and acts on the first rule that
hits. It is the frame the rules run in: the checks, the actions, the record they leave, a dry run,
and `/automod status`. The code is in
[`crates/closure-mod-automod`](../crates/closure-mod-automod/src/lib.rs); its tests are in
[`tests/automod.rs`](../crates/closure-mod-automod/tests/automod.rs). It is off until a server turns
it on.

## Before you turn it on

- **Message Content Intent.** In the developer portal, under *Bot* → *Privileged Gateway Intents*,
  switch on **Message Content**. The module asks for it at every start, and Discord refuses the
  connection without it. Without it the bot sees no text: the word, link, invite, caps, repeat and
  emoji checks never hit. Mentions, attachments and flood still work.
- Permissions: **Manage Messages** (to delete), **Kick Members**, **Ban Members**,
  **Moderate Members** (timeouts), **Manage Roles** (for the `role` method), and **Send Messages**
  in the log channel and in any `notify` channel.
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
        - id: spoilers
          check: { type: words, words: ["spoiler"], match: contains }
          actions: [{ do: delete }, { do: notify, channel: "123456789012345678" }]
```

Start with `dry_run: true`, read the log for a while, then turn it off.

## Configuration

| Key | Default | Meaning |
| --- | --- | --- |
| `dry_run` | `false` | Act on nothing; post what would have happened to the log channel. |
| `warn_expire_days` | `30` | After how many days a `warn` action expires (1 to 365). |
| `dm_on_action` | `true` | Whether the member is messaged about a punishment. |
| `timeout_method` | `discord` | `discord`, or `role`: a role that a timer takes off again. |
| `timeout_role` | none | The role id a `role` timeout gives. |
| `rules` | `[]` | At most 50 rules, each checked in order. |

A rule has an `id` (1 to 32 characters: lowercase letters, digits, `_`, `-`, unique in the list), an
optional `name` shown in logs (defaults to the id), `enabled` (default `true`), a `check`, an
optional `scope`, and `actions` (1 to 8, run in order).

### Checks

Each check has a `type`. Every check is written once below with a rule that uses it.

```yaml
# words: a list of words; match is contains (anywhere) or whole (the default)
{ type: words, words: ["spoiler", "バカ"], match: contains }

# links: any link (mode any), any link outside the domains (allowlist), or inside them (blocklist)
{ type: links, mode: allowlist, domains: ["example.com"] }

# invites: any invite whose code is not in allow
{ type: invites, allow: ["abcDEF"] }

# mentions: more than max mentioned users, roles and @everyone together
{ type: mentions, max: 5 }

# caps: more than max_ratio of the letters are capitals, with at least min_letters letters (10)
{ type: caps, max_ratio: 0.8, min_letters: 10 }

# repeat: one character repeated more than max_run times in a row
{ type: repeat, max_run: 6 }

# emoji: more than max emoji (Unicode and custom)
{ type: emoji, max: 10 }

# attachments: more than max files, or a file whose extension is blocked (either or both)
{ type: attachments, max: 3, blocked_extensions: ["exe", "bat"] }

# flood: messages new messages from one author within per_minutes (whole minutes); counted per rule
{ type: flood, messages: 5, per_minutes: 1 }
```

A `links` domain also matches its subdomains: `example.com` matches `shop.example.com`, not
`evil-example.com`.

### Scope

```yaml
scope:
  channels: ["111111111111111111"]        # only these channels (empty: all)
  exclude_channels: ["222222222222222222"]
  roles: ["333333333333333333"]           # only members with one of these roles (empty: all)
  exclude_roles: ["444444444444444444"]
  include_bots: false                     # bots are left alone unless this is true
  exempt_moderators: true                 # members at the moderator level are not checked
```

The server owner is never checked, and neither is the bot's own message.

### Actions

| Action | What it does |
| --- | --- |
| `{ do: delete }` | Deletes the message. A message that is gone already is not an error. |
| `{ do: warn }` | Warns the member; the warning expires after `warn_expire_days`. |
| `{ do: timeout, duration: 1h }` | Times the member out. At most 28 days with Discord's timeout. |
| `{ do: kick }` | Kicks the member. |
| `{ do: ban, duration: 7d, delete_days: 1 }` | Bans the member; `duration` is optional (forever when left out), `delete_days` is 0 to 7. |
| `{ do: notify, channel: "…" }` | Posts one embed to the channel: the rule, the member, what was found, and a link to the message. |

Durations read like `30m`, `1h`, `7d`. The actions run in the order given. A failing action does
not stop the next one; the first error is logged.

The reason of every punishment and record is `rule name: what was found`, in the server's language.

### Order of the rules

The rules are checked in order, and the **first rule that hits** acts. The rest are not looked at
for that message.

## What a hit leaves behind

- A punishment (`warn`, `timeout`, `kick`, `ban`) is recorded as its own case, as a moderator's
  command would record it, and posted to the log.
- A rule that only deletes or notifies leaves an `automod` case, so that the deletion shows in the
  log.
- An edit of a message is checked the same way as a new message (an edit with neither text nor
  files is skipped).

## Dry run

With `dry_run: true` nothing is done: no deletion, no punishment, no case, and a flood counter is
not reset. For each hit, one embed titled `[dry run] automod` goes to the server's log channel,
with the rule, the member, what was found, and the actions that would have run. Without a log
channel a dry run does nothing at all.

## Status

`/automod status` (privately) shows whether dry run is on, how many rules are enabled, disabled and
invalid, each rule on a line, and each invalid rule with the reason. Invalid rules never run.

A rule is invalid when its shape is wrong, a value is out of range, its id is taken by an earlier
valid rule, or its check or action is unusable. The schema refuses such a rule when it is saved;
the module still lists it if one got in some other way.

## Limits

- The rules see only the message itself. There is no message history and no cache, so a rule cannot
  look at what a member said before.
- A flood counts whole minutes: `per_minutes` rounds up to the minute. A message that hits an
  earlier rule is not counted by a later flood rule, because the first hit ends the check.
- Edits that only add embeds (link previews, say) are not delivered by the gateway library, so
  they are not checked.
- Japanese text has no spaces, so `whole` does not match a Japanese word inside a sentence. Prefer
  `contains` for Japanese.
- Administrators are not known from a message. Only the moderator level counts for the exemption,
  so an administrator without a moderator role is checked like a member.
- The rules are read on every message, so the work grows with their number.
