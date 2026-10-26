# Diagnostics (`diagnostics`)

Liveness and version commands.

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `respond_to_interactions`

## Permissions

This module declares no permissions.

## Commands

### `/ping`

Check that the bot is alive

- Japanese name: `ping`
- Japanese description: Bot が動作しているか確認します

This command takes no options.

### `/about`

Show the version and where to find the source code

- Japanese name: `about`
- Japanese description: バージョンとソースコードの場所を表示します

This command takes no options.

## Configuration

Set under `modules.diagnostics.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `ephemeral` | boolean | `true` | - | Only show replies to the person who ran the command. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |

Default configuration:

```json
{
  "ephemeral": true
}
```
