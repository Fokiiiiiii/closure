# Verify (`verify`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `read_guild`
- `read_bans`
- `kick_members`
- `manage_roles`
- `respond_to_interactions`
- `storage`
- `schedule`
- `cases`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `verify.panel` | 75 | Post the verification panel |

## Commands

### `/verify`

Post the panel members press to get in

- Japanese name: `verify`
- Japanese description: 参加するために押すパネルを投稿します

This command takes no options.

## Configuration

Set under `modules.verify.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `kick_after` | any | `null` | - | How long a member has to verify before being kicked, such as 1d. Left empty, never. |
| `mode` | string | `"button"` | - | What a member does: press the button, or answer a small sum first. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `role` | any | `null` | role | The role a verified member has. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |
| `unverified_role` | any | `null` | role | A role a member has from the join until verified. Left empty, members get nothing at the join. |

Default configuration:

```json
{
  "kick_after": null,
  "mode": "button",
  "role": null,
  "unverified_role": null
}
```
