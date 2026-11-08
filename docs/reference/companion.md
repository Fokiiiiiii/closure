# Companion channels (`companion`)

A text channel that only the people in a voice channel can see.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `read_guild`
- `manage_channels`
- `respond_to_interactions`
- `storage`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.companion.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `pairs` | array | `[]` | - | Pairs of a voice channel and its text channel. Hide the text channel from everyone else with its own permissions. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "pairs": []
}
```
