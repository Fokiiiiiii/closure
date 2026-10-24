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

limits:                    # override single values of the preset
  handler_timeout_secs: 20

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
| `limits` | Per-server resource limits; see below. |
| `modules.<id>` | `enabled`, an optional `capabilities` list, and `config` — validated against the module's own JSON Schema. |

Unknown keys are errors, so a typo is reported instead of silently ignored. In a module's `config`,
`null` means "use the default".

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
document; `required`, `minProperties`, `maxProperties`, `propertyNames`, `unevaluatedProperties`,
`dependentRequired`, `dependentSchemas` and `not` are dropped, and so is a `const` or `enum` that
holds an object (a sparse override is never equal to one); `oneOf` and `if`/`then`/`else` are
weakened to "any of the branches"; and an unknown key may only be `null`. The items of a list are
replaced as a whole and stay strict.

A module's schema may use `$defs`/`definitions` and `$ref` (as `schemars` writes them, with the
escapes of a URI fragment and a JSON pointer, and also `#` for the root); each module's schema is
its own reference scope, so two modules may define a type of the same name. A schema that names
itself with `$id` keeps its name, and no two modules may use the same one. The bot refuses to
start with a module schema it cannot translate: `$anchor`, `$dynamicRef` and the like, a `$ref`
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
