# Logs (`logs`)

Post the edits and deletions of messages, and the joins, leaves and bans of members, to a channel.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `respond_to_interactions`
- `message_history`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.logs.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `bans` | boolean | `true` | - | Post the bans and unbans of members. |
| `channel` | any | `null` | - | The channel the entries are posted to. Left out, the guild's log channel. |
| `member_joins` | boolean | `true` | - | Post the joins of members. |
| `member_leaves` | boolean | `true` | - | Post the departures of members. |
| `message_deletes` | boolean | `true` | - | Post the deletions of messages, with their text and attachments as the history kept them. |
| `message_edits` | boolean | `true` | - | Post the edits of messages. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |

Default configuration:

```json
{
  "bans": true,
  "channel": null,
  "member_joins": true,
  "member_leaves": true,
  "message_deletes": true,
  "message_edits": true
}
```
