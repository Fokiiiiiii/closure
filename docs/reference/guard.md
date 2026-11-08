# Guard (`guard`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `read_guild`
- `kick_members`
- `manage_roles`
- `read_audit_log`
- `respond_to_interactions`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.guard.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `alert_channel` | any | `null` | channel | Where the staff are told. Left empty, the log channel of the server. |
| `bans` | integer | `10` | - | Members one account may ban within the window before it is stopped. 0: not watched. The bot's own bans are not counted. |
| `channel_deletes` | integer | `3` | - | Channels one account may delete within the window before it is stopped. 0: not watched. |
| `exempt_roles` | array | `[]` | - | Roles whose members are never stopped. |
| `exempt_users` | array | `[]` | - | Accounts that are never stopped. The owner of the server and the bot are never stopped. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `response` | string | `"strip"` | - | What is done to an account that reaches a limit: strip its moderator powers (a bot is kicked) and tell the staff, or only tell the staff. |
| `role_deletes` | integer | `3` | - | Roles one account may delete within the window before it is stopped. 0: not watched. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |
| `window` | string | `"30s"` | - | The window in which the changes are counted, such as 30s. |

Default configuration:

```json
{
  "alert_channel": null,
  "bans": 10,
  "channel_deletes": 3,
  "exempt_roles": [],
  "exempt_users": [],
  "response": "strip",
  "role_deletes": 3,
  "window": "30s"
}
```
