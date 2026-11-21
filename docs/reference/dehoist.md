# Dehoist (`dehoist`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `read_guild`
- `manage_nicknames`
- `respond_to_interactions`
- `storage`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `dehoist.sweep` | 50 | Clean the names of the members already in the server |

## Commands

### `/dehoist`

Strip leading symbols from names that sort first

- Japanese name: `ホイスト解除`
- Japanese description: 名前の先頭の記号を外す

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
