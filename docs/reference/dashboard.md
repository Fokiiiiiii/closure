# Dashboard (`dashboard`)

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `respond_to_interactions`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `dashboard.edit` | 75 | Edit dashboard settings. |

## Commands

This module declares no commands.

## Configuration

Set under `modules.dashboard.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | - | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{}
```
