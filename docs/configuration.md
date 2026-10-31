# Configuration

Every server is configured by one document. There are two ways to edit it:

- **Simple:** pick a `preset` and flip a few switches (typically from a dashboard form).
- **Advanced:** write the whole thing as YAML.

Both edit the same *sparse* document: only what differs from the defaults is stored. Fields you do
not write keep following the defaults, so when a default improves, every server that did not
override it gets the improvement.

## The document

```yaml
version: 1                 # schema version; omit for the current one
preset: large              # small | medium | large   (default: medium)
language: ja               # auto | en | ja           (default: auto)
log_channel: 123456789012345678
moderator_roles: [111111111111111111, 222222222222222222]
timezone: Asia/Tokyo       # the zone of a date typed without one
prefix: "!"                # text commands; unset, they are off
prefix_delete_command_after: 5   # delete a text command 5 seconds after its reply; unset, kept
prefix_delete_reply_after: 30    # delete the reply 30 seconds after it is sent; unset, kept

limits:                    # override single values of the preset
  handler_timeout_secs: 20

permissions:               # who may do what; see below
  roles: { 111111111111111111: moderator, 333333333333333333: trusted }
  keys:
    moderation.ban: { min_level: admin, deny: { users: [444444444444444444] } }

modules:
  diagnostics:
    enabled: true
    capabilities: [respond_to_interactions]   # can only narrow what the module declares
    config:
      ephemeral: false     # the module's own section
```

| Field | Meaning |
| --- | --- |
| `version` | Schema version. A document written by a newer closure is refused rather than misread. |
| `preset` | Sizes the limits below for a small, medium or large server. |
| `language` | The language of the bot's messages. `auto` follows the community language the server chose in Discord. |
| `log_channel` | Where modules post moderation logs. |
| `moderator_roles` | Roles whose members may use moderation features (at most 100). |
| `timezone` | The zone of a date and time typed in a command without one; see [time zone, prefix and emojis](#time-zone-prefix-and-emojis). |
| `prefix` | The prefix of text commands, 1 to 10 characters without spaces. Unset, text commands are off. |
| `prefix_delete_command_after`, `prefix_delete_reply_after` | Delete a text command's message, or its reply, this many seconds after the reply is sent: 1 to 86400. Unset, it is kept. |
| `success_emoji`, `error_emoji` | The emoji in front of success and error replies; see [time zone, prefix and emojis](#time-zone-prefix-and-emojis). |
| `limits` | Per-server resource limits; see below. |
| `permissions` | Levels of roles and members, and what is decided about single permission keys; see below. |
| `modules.<id>` | `enabled`, an optional `capabilities` list, and `config` — validated against the module's own JSON Schema. |

Unknown keys are errors, so a typo is reported instead of silently ignored. In a module's `config`,
`null` means "use the default".

### Permissions

A member has a **level** from 1 to 100, and a module's actions are guarded by **permission keys**
such as `moderation.ban`. A module declares its keys with the level each needs by default; the key
always starts with the id of the module that declares it. A level is written as a number or as a
name: `general` (1), `trusted` (25), `moderator` (50), `admin` (75), `owner` (100).

A member's level is the highest that any of these gives: each of their roles (`permissions.roles`),
their own entry (`permissions.users`), Discord's Administrator permission
(`permissions.administrator_level`, `admin` unless changed; `general` makes it count for nothing),
and, for the owner of the server, always 100. `moderator_roles` still works and gives those roles
level `moderator`; an entry in `permissions.roles` for the same role wins.

For one key, `permissions.keys.<key>` may set `min_level` (instead of the module's default),
`allow` (roles and users who may use it whatever their level) and `deny` (who may never use it).
`deny` beats `allow` and `allow` beats the level; the owner of the server is never refused. A key
no module declares is an error, so a typo is reported. Each list holds at most 200 entries.

### Time zone, prefix and emojis

`timezone` is the zone of a date and time typed in a command without one, such as `2026-11-01 18:00`
(see [durations as a date and time](moderation.md#durations-as-a-date-and-time)). Without it, a
Japanese server reads such a time as UTC+09:00 and any other server as UTC. It is one of:

- `UTC`, `Z`, `GMT`, or an offset: `UTC+9`, `UTC+09:00`, `UTC-5`, `GMT+9`, `+09:00`, `-0500`. The
  offset is between `UTC-12:00` and `UTC+14:00`.
- A fixed zone: `Asia/Tokyo`, `Asia/Seoul` (+9), `Asia/Shanghai`, `Asia/Hong_Kong`, `Asia/Taipei`,
  `Asia/Singapore` (+8), `Asia/Bangkok`, `Asia/Jakarta` (+7), `Asia/Kolkata` (+5:30), `Asia/Dubai` (+4).

`UTC`, `GMT` and `Z` (and the `UTC` or `GMT` before an offset) are accepted in any capitalisation, and
surrounding spaces are ignored. The zone names are case-sensitive: `Asia/Tokyo`, not `asia/tokyo`.

A zone with daylight saving time, such as `America/New_York` or `Europe/London`, is refused, because a
name cannot follow the change of the clocks. Write its offset for now and change it when the clocks
change:

```yaml
timezone: UTC-5    # America/New_York in winter; use UTC-4 from March to November
```

`prefix` is 1 to 10 characters without spaces, such as `!` or `>>`. A message starting with it runs
the slash command of the same name (`!warn @user spam`); see [text commands](writing-modules.md#text-commands).
An empty prefix is refused: leave the key out to turn text commands off.

`prefix_delete_command_after` and `prefix_delete_reply_after` set the delays after which the message
that ran a text command and its reply are deleted, each on its own. The bot may always delete its own
reply. Deleting the command message needs the Manage Messages permission; without it the message is
kept, and the failure is logged at debug level. A command whose answer is deferred and then sent by an
edit is not covered: only an answer sent at once is.

`success_emoji` and `error_emoji` go in front of the text of a success or an error reply, followed by a
space, so `✅ Warned user 7` reads as the success it is. The default is `✅` and `❌`. Each is a Unicode
emoji (`❤️`, `👍🏽`) or a custom one as `<:name:id>` or `<a:name:id>`, at most 100 characters. An empty
string (`""`) puts no emoji there. Info replies and warnings never get one.

### Module texts

`texts` in a module's `config` replaces the messages that module shows: replies, direct messages,
warnings, log headings. Each key is a message id of the module's locale file, such as `warned` in
the moderation module, and the text replaces the message in every language. Inside the text,
`{name}` is replaced by the message's argument `name` (`{user}`, `{number}`, ...); a `{..}` that names
no argument stays as written.

```yaml
modules:
  moderation:
    config:
      texts:
        warned: "Warned {user}: case #{number}, {active} active. The next one is a timeout."
```

An empty string, or no entry for the id, keeps the module's own text. A value that is not a string,
and an id that no message has, are ignored, so they never stop the configuration from loading. A
text longer than 2000 characters is cut to 2000, ending with `…`. An id that the module and the
shared `core` messages both use is replaced in both.

### Bot-wide defaults

`--bot-config <file>` (or `BOT_CONFIG`) names a document of this same form that every server
starts from: the preset, then this file, then the server's own document, key by key (`null` in the
server's document means "use the bot's value"). The layers are applied one after the other: a
module's configuration is its defaults, with the bot's `config` merged over them, with the server's
merged over that, so a layer that replaces an object by a number (or the other way round) leaves
nothing of the layers below it. A Discord id in `permissions.roles` and `permissions.users` is the
same id however it is written (`5`, `05`); writing it twice in one document is refused. The file is read once at start-up and must be a
valid configuration on its own, or the bot does not start.

### Presets and limits

| | `small` | `medium` | `large` |
| --- | --- | --- | --- |
| `handler_timeout_secs` — how long a module may spend on one event | 10 | 10 | 15 |
| `queue_capacity` — events the server may have waiting or running at once | 64 | 256 | 1024 |
| `concurrency` — independent lanes handled in parallel | 4 | 8 | 32 |

Allowed ranges: timeout 1–60, queue 8–10 000, concurrency 1–256.

Beyond `queue_capacity` new events are dropped (and counted in the metrics) rather than queued
without limit. The budget belongs to the whole server, so one busy channel may use all of it.
Slash commands have a budget of the same size and extra lanes (a quarter of `concurrency`, at
least one) of their own, so a flood of ordinary events cannot delay them past Discord's three
second window. A new `handler_timeout_secs` and `queue_capacity` apply to the next event; a new
`concurrency` takes effect once the server's queue has drained.

## Checking a document

`GET /v1/schema` returns the JSON Schema of the whole document, including every module's section.
A document holds only what differs from the defaults, so the published schema is the one of an
**override**, made from each module's own schema. Its contract: it never rejects an override
that the bot accepts. It may let one through that the bot then refuses, because the bot judges
the result of merging the override into the defaults and the schema only sees the override (for
example an object that lacks a property no default fills in).

To keep that contract, wherever values are merged key by key: `null` is accepted for any
property (it means "the default"), for the whole `config`, and for every optional field of the
document (which may itself be `null`, as may a module's entry); `required`, `minProperties`,
`maxProperties`, `propertyNames`, `unevaluatedProperties`, `unevaluatedItems`,
`dependentRequired`, `dependentSchemas`, `dependencies` and `not` are dropped, and so is a
`const` or `enum` that holds an object (a sparse override is never equal to one); `oneOf` and
`if`/`then`/`else` are weakened to "any of the branches"; and an unknown key may only be `null`.
The items of a list are replaced as a whole and stay strict, and so does a `$ref: "#"` reached
from one. The document also takes what the parser takes in other spellings: a language in any
capitalisation or with a region (`ja-JP`), a Discord id as a number or as digits in a string, a
capability listed twice, the text of a number or a boolean where one is wanted (`"2"`, `"yes"`),
and a preset or capability written as a map holding `null` (`{ small: null }`).

Keywords that start with `x-` are annotations: the bot passes them on unchanged. The `x-ui` hints
for the dashboard's forms are the one checked by shape (see [writing a module](writing-modules.md#hints-for-the-dashboard-x-ui)).

A module's schema may use `$defs`/`definitions` and `$ref` (as `schemars` writes them, with the
escapes of a URI fragment and a JSON pointer, and also `#` for the root); each module's schema is
its own reference scope, so two modules may define a type of the same name. A schema that names
itself with `$id` has it replaced by one the bot assigns (`urn:closure:module:<id>`), since it
may only refer to itself. The bot refuses to start with a module schema it cannot translate:
`$anchor`, `$dynamicRef` and the like, a `$ref`
that is not `#` or one name below `$defs`/`definitions`, a `$id` or `$schema` anywhere but at the
top, or a `$schema` other than draft 2020-12. What the bot does not know as a keyword is treated
as an annotation. Nesting and reference chains deeper than 128 levels allow anything.
Use it for editor completion or to generate forms.

`POST /v1/guilds/{id}/config/validate` checks a document without storing it and returns what it
resolves to. `GET /v1/guilds/{id}/config/effective` shows the complete configuration of a server
with every default filled in.

Problems are reported all at once, with a path and a stable code:

```json
{ "error": { "code": "invalid_config", "message": "the configuration has problems",
  "issues": [ { "path": "modules.warn.config.limit", "code": "invalid_module_config",
                "message": "99 is greater than the maximum of 10" } ] } }
```

Syntax errors carry the 1-based `line` and `column`.

## Using the HTTP API

All of these need `Authorization: Bearer $API_ADMIN_TOKEN`. Make the token long and random, for
example `openssl rand -hex 24`; only its length (at least 24 characters) is checked.

```bash
# read (YAML if you ask for it, JSON otherwise); the ETag is the revision
curl -i -H "Authorization: Bearer $API_ADMIN_TOKEN" \
     -H "Accept: application/yaml" \
     http://127.0.0.1:8080/v1/guilds/$GUILD/config

# write: If-Match says which revision you edited ("0" when none exists yet)
curl -i -X PUT -H "Authorization: Bearer $API_ADMIN_TOKEN" \
     -H 'If-Match: "0"' --data-binary @config.yaml \
     http://127.0.0.1:8080/v1/guilds/$GUILD/config
```

- A write without `If-Match` is refused (`428`).
- If someone else saved in between, the write is refused (`412`) and the response's `ETag` is the
  current revision; reload and merge.
- Invalid documents are refused (`422`) and nothing is stored.
- Only servers the bot is currently in can be configured (`404` otherwise).
- A successful write takes effect at once in the running bot (the lane count is the one exception
  noted above) and is recorded in the history
  (`GET /v1/guilds/{id}/config/history`) and the audit trail.

If you edit the database by hand and delete or recreate a server's configuration row, restart the
bot: it ignores a revision lower than the one it already holds.

Both YAML and JSON are accepted as the request body (JSON is YAML). Bodies are limited to 64 KiB,
YAML aliases are capped, and a parsed document may not exceed 128 KiB, so a hostile document
cannot exhaust memory. Text containing the NUL character is refused with a `422`.

## Simple and advanced views

The same document can be read and written in two views. The **basic** view holds the settings a
simple form edits: `preset`, `language`, `timezone`, `prefix`, `success_emoji`, `error_emoji`,
`log_channel`, `moderator_roles`, each module's `enabled` switch, and the settings a module marks
basic (`"x-ui": { "level": "basic" }`, see [writing modules](writing-modules.md#simple-and-advanced-settings-xuilevel)).
The **advanced** view is everything, as before. Without a `view` parameter a request is advanced.

| Request | Basic view | Advanced view (default) |
| --- | --- | --- |
| `GET /v1/schema?view=basic` | the schema of the basic settings only | the whole schema |
| `GET /v1/guilds/{id}/config?view=basic` | only the basic part of the stored document | the whole document |
| `PUT /v1/guilds/{id}/config?view=basic` | a patch on top of the stored document | the whole document, replaced |
| `POST /v1/guilds/{id}/config/validate?view=basic` | the patch merged onto the stored document, then checked | the body checked on its own |
| `GET /v1/guilds/{id}/config/effective` | ignores `view`: the complete configuration in both | the same |

A `view` other than `basic` or `advanced` is a `400`.

A basic write is a patch. Keys present are set, keys absent keep their stored value, and `null`
clears the key, as it does in a full document. A key outside the basic view (`limits`,
`permissions`, a module's `capabilities`, or a module setting that is not marked basic) is refused
with a `422` whose issue names the key with the code `not_basic`; nothing is stored. Advanced values
are never changed by a basic write. The merged document then goes through the same validation,
storage, history and audit path as a full write, and the `If-Match` rule is unchanged: the patch
applies to the revision it names, and a stale one is refused with `412`.

```bash
# the simple settings only; the same ETag and revision rules as the full document
curl -s -H "Authorization: Bearer $API_ADMIN_TOKEN" \
     "http://127.0.0.1:8080/v1/guilds/$GUILD/config?view=basic"

# change two simple settings; the limits and the advanced module settings stay as they are
curl -i -X PUT -H "Authorization: Bearer $API_ADMIN_TOKEN" \
     -H 'If-Match: "3"' --data-binary $'preset: large\nmodules:\n  diagnostics:\n    config: { ephemeral: false }\n' \
     "http://127.0.0.1:8080/v1/guilds/$GUILD/config?view=basic"

# the form for the simple settings
curl -s -H "Authorization: Bearer $API_ADMIN_TOKEN" \
     "http://127.0.0.1:8080/v1/schema?view=basic"
```

A dashboard shows its simple form from the basic view and offers the full editor from the
advanced one. Switching is only a change of the `view` parameter: both views edit the same stored
document, so nothing is lost by going back and forth.

Two decisions of this design:

- The format stamp `version` is accepted in both views, since a YAML document read in one view
  carries it. It is not a setting.
- A basic write is recorded in the history and the audit trail like any other write. The API has
  no user identity of its own (one admin token), so the actor is not recorded, as for a full write.
