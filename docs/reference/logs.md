# Logs (`logs`)

Post the edits and deletions of messages, and the joins, leaves and bans of members, to a channel.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `respond_to_interactions`
- `message_history`
- `keep_attachments`

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
| `keep_attachments` | boolean | `false` | - | Keep the files of attachments as their messages arrive, not only when a message is deleted or edited. Uses storage: a server's files are capped and kept for the message history's retention. |
| `member_joins` | boolean | `true` | - | Post the joins of members. |
| `member_leaves` | boolean | `true` | - | Post the departures of members. |
| `message_deletes` | boolean | `true` | - | Post the deletions of messages, with their text and files as the history kept them. |
| `message_edits` | boolean | `true` | - | Post the edits of messages. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | - | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "bans": true,
  "channel": null,
  "keep_attachments": false,
  "member_joins": true,
  "member_leaves": true,
  "message_deletes": true,
  "message_edits": true
}
```
