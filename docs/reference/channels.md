# Channels (`channels`)

Automatically delete messages or add reactions in selected channels.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `add_reactions`
- `manage_messages`
- `respond_to_interactions`
- `schedule`
- `read_messages`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.channels.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `auto_delete` | array | `[]` | - | Channels whose new messages are deleted after a delay. |
| `auto_react` | array | `[]` | - | Channels whose new messages receive reactions. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | - | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "auto_delete": [],
  "auto_react": []
}
```
