# Phishing (`phishing`)

Removes messages whose links an external service flags as phishing or scams.

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `send_messages`
- `manage_messages`
- `timeout_members`
- `respond_to_interactions`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.phishing.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `notice` | boolean | `true` | - | Post a short notice where the message was. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |
| `timeout_minutes` | integer | `10` | - | Minutes the author is timed out for after posting such a link; 0 leaves them be. |

Default configuration:

```json
{
  "notice": true,
  "timeout_minutes": 10
}
```
