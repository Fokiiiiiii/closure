# Automod (`automod`)

Automatic moderation: rules that check every message, and act on the first one that hits.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `manage_messages`
- `read_guild`
- `kick_members`
- `ban_members`
- `timeout_members`
- `manage_roles`
- `respond_to_interactions`
- `storage`
- `schedule`
- `cases`
- `send_direct_messages`
- `message_history`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `automod.view` | 50 | See the rules and the state of automod |

## Commands

### `/automod`

Automatic moderation: its rules and state

- Japanese name: `自動モデレーション`
- Japanese description: 自動モデレーションのルールと状態

#### `/automod status`

Show whether dry run is on and which rules run

- Japanese name: `状態`
- Japanese description: ドライランの設定と、動いているルールを表示します

## Configuration

Set under `modules.automod.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `dm_on_action` | boolean | `true` | - | Whether the member gets a direct message about a punishment. |
| `dry_run` | boolean | `false` | - | Act on nothing: post what would have happened to the log channel. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `rules` | object | `{}` | - | The rules, keyed by id. Each one is checked on every message; the rules are checked in the order of their ids, and the first one that trips acts unless it says to continue. |
| `timeout_method` | string | `"discord"` | - | How a timeout is given: Discord's timeout, or a role that is taken off when its time is up. |
| `timeout_role` | string or null | `null` | - | The id of the role a timeout gives, when timeout_method is role. |
| `warn_expire_days` | integer | `30` | - | After how many days a warning of a rule expires. |

Default configuration:

```json
{
  "dm_on_action": true,
  "dry_run": false,
  "rules": {},
  "timeout_method": "discord",
  "timeout_role": null,
  "warn_expire_days": 30
}
```
