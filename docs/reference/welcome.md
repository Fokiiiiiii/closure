# Welcome (`welcome`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `manage_messages`
- `read_guild`
- `respond_to_interactions`
- `schedule`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.welcome.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `channel` | string or null | `null` | channel | welcome-channel |
| `delete_after` | string or null | `null` | - | welcome-delete-after |
| `include_bots` | boolean | `false` | - | welcome-include-bots |
| `message` | string or null | `null` | - | welcome-message |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | - | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "channel": null,
  "delete_after": null,
  "include_bots": false,
  "message": null
}
```
