# Persist (`persist`)

Give a member back their roles and nickname when they rejoin.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `manage_nicknames`
- `manage_roles`
- `respond_to_interactions`
- `storage`

## Permissions

This module declares no permissions.

## Commands

This module declares no commands.

## Configuration

Set under `modules.persist.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `nickname` | boolean | `false` | - | Whether the nickname comes back too. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `roles` | array | `[]` | role | The roles to give back to a member who rejoins. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "nickname": false,
  "roles": []
}
```
