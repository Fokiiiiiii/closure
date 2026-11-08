# Sticky (`sticky`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `manage_messages`
- `respond_to_interactions`
- `storage`
- `schedule`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.sticky.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `delay` | string | `"5s"` | - | How long after a message the text is posted again, such as 5s. A busy channel gets the text once per delay. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `stickies` | array | `[]` | - | Texts that stay at the bottom of a channel: the channel, and the text. Messages of bots do not move them. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "delay": "5s",
  "stickies": []
}
```
