# Logs (`logs`)

Post the edits and deletions of messages, the changes of channels, threads, roles, members and voice, and of emojis, stickers and stages, to a channel.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `read_guild`
- `read_audit_log`
- `respond_to_interactions`
- `storage`
- `schedule`
- `message_history`
- `keep_attachments`
- `manage_invites`

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
| `channels` | boolean | `true` | - | Post the creation, changes and removal of channels. |
| `emojis` | boolean | `true` | - | Post the creation, changes and removal of emojis. |
| `ignored_channels` | array | `[]` | - | Channels nothing is logged about: their messages, and their own creation, changes and removal. Threads count with their channel. |
| `ignored_roles` | array | `[]` | - | Roles nothing is logged about: changes to the role, the role changes of members who gain or lose it, and the edits of messages by members who hold it. |
| `ignored_users` | array | `[]` | - | Members nothing is logged about: their messages, joins, departures, bans, nicknames, timeouts, roles and voice. |
| `invite_tracking` | boolean | `true` | - | Name the invite a member joined with in the join entry. Needs the Manage Server permission, and is a guess when several members join at once. |
| `keep_attachments` | boolean | `false` | - | Keep the files of attachments as their messages arrive, not only when a message is deleted or edited. Uses storage: a server's files are capped and kept for the message history's retention. |
| `member_joins` | boolean | `true` | - | Post the joins of members. |
| `member_leaves` | boolean | `true` | - | Post the departures of members. |
| `member_nicknames` | boolean | `true` | - | Post the nickname changes of members. |
| `member_roles` | boolean | `true` | - | Post the role changes of members. |
| `member_timeouts` | boolean | `true` | - | Post the start and end of member timeouts. |
| `message_deletes` | boolean | `true` | - | Post the deletions of messages, with their text and files as the history kept them. |
| `message_edits` | boolean | `true` | - | Post the edits of messages. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `roles` | boolean | `true` | - | Post the creation, changes and removal of roles. |
| `routes` | object | `{"members":null,"messages":null,"moderation":null,"server":null,"voice":null}` | - | A channel of their own for kinds of entries. A kind left empty goes where the channel or webhook above says. |
| `stage_instances` | boolean | `true` | - | Post the start, changes and end of stage instances. |
| `stickers` | boolean | `true` | - | Post the creation, changes and removal of stickers. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |
| `threads` | boolean | `true` | - | Post the creation, changes and removal of threads. |
| `voice` | boolean | `false` | - | Post members joining, leaving and moving between voice channels. |
| `webhook` | any | `null` | - | The named webhook the entries are posted with. |
| `webhook_routes` | object | `{"members":null,"messages":null,"moderation":null,"server":null,"voice":null}` | - | Webhooks of their own for kinds of entries, by name. A webhook comes before the channel of the same kind; if it is gone, the entry goes to that channel, or else to the usual one. |

Default configuration:

```json
{
  "bans": true,
  "channel": null,
  "channels": true,
  "emojis": true,
  "ignored_channels": [],
  "ignored_roles": [],
  "ignored_users": [],
  "invite_tracking": true,
  "keep_attachments": false,
  "member_joins": true,
  "member_leaves": true,
  "member_nicknames": true,
  "member_roles": true,
  "member_timeouts": true,
  "message_deletes": true,
  "message_edits": true,
  "roles": true,
  "routes": {
    "members": null,
    "messages": null,
    "moderation": null,
    "server": null,
    "voice": null
  },
  "stage_instances": true,
  "stickers": true,
  "threads": true,
  "voice": false,
  "webhook": null,
  "webhook_routes": {
    "members": null,
    "messages": null,
    "moderation": null,
    "server": null,
    "voice": null
  }
}
```
