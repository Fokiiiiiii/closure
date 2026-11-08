# Dehoist (`dehoist`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `read_guild`
- `manage_nicknames`
- `respond_to_interactions`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `dehoist.sweep` | 50 | Clean the names of the members already in the server |

## Commands

### `/dehoist`

Take leading symbols off the names that sort to the top of the member list

- Japanese name: `ホイスト解除`
- Japanese description: メンバー一覧の上位に居座る、記号で始まる名前から先頭の記号を外します

This command takes no options.

## Configuration

Set under `modules.dehoist.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{}
```
