# Phishing (`phishing`)

Bans or kicks members who post links an external service flags as phishing or scams, and removes the message.

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `manage_messages`
- `kick_members`
- `ban_members`
- `respond_to_interactions`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.phishing.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `action` | string | `"ban"` | - | What happens to the member who posts such a link: ban (with their last hour of messages removed) or kick. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "action": "ban"
}
```
