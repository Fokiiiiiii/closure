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
- `manage_nicknames`
- `manage_roles`
- `manage_channels`
- `respond_to_interactions`
- `storage`
- `schedule`
- `cases`
- `send_direct_messages`
- `message_history`
- `manage_guild`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `automod.view` | 50 | See the rules and the state of automod |
| `automod.counters` | 50 | Look at and change the counters of automod |
| `automod.antiraid.view` | 50 | See the antiraid level and its quarantine |
| `automod.antiraid.set` | 75 | Raise or lower the antiraid level |
| `automod.antiraid.release` | 50 | Release members from the antiraid quarantine |

## Commands

### `/automod`

Automatic moderation: its rules and state

- Japanese name: `自動モデレーション`
- Japanese description: 自動モデレーションのルールと状態

#### `/automod status`

Show whether dry run is on and which rules run

- Japanese name: `状態`
- Japanese description: ドライランの設定と、動いているルールを表示します

#### `/automod counters`

Look at and change the counters of automod

- Japanese name: `カウンター`
- Japanese description: automod のカウンターを見る・変更する

##### `/automod counters view`

Show the value of a counter

- Japanese name: `view`
- Japanese description: カウンターの値を表示する

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `counter` | counter | string | yes | - | The counter |
| `user` | user | user | no | - | The member the counter is kept for |
| `channel` | channel | channel | no | - | The channel the counter is kept for |

##### `/automod counters set`

Set the value of a counter

- Japanese name: `set`
- Japanese description: カウンターの値を設定する

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `counter` | counter | string | yes | - | The counter |
| `value` | value | integer | yes | - | The new value |
| `user` | user | user | no | - | The member the counter is kept for |
| `channel` | channel | channel | no | - | The channel the counter is kept for |

##### `/automod counters add`

Add to a counter

- Japanese name: `add`
- Japanese description: カウンターに加算する

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `counter` | counter | string | yes | - | The counter |
| `amount` | amount | integer | yes | - | How much to add; a negative number takes away |
| `user` | user | user | no | - | The member the counter is kept for |
| `channel` | channel | channel | no | - | The channel the counter is kept for |

##### `/automod counters reset`

Put a counter back to its start

- Japanese name: `reset`
- Japanese description: カウンターを初期値に戻す

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `counter` | counter | string | yes | - | The counter |
| `user` | user | user | no | - | The member the counter is kept for; with no scope given, every scope is reset |
| `channel` | channel | channel | no | - | The channel the counter is kept for; with no scope given, every scope is reset |

#### `/automod antiraid`

Raise or lower the antiraid level, and release the quarantined members

- Japanese name: `アンレイド`
- Japanese description: アンレイドのレベルを上げ下げし、隔離中のメンバーを解除する

##### `/automod antiraid view`

Show the antiraid level and the quarantine

- Japanese name: `view`
- Japanese description: アンレイドのレベルと隔離を表示する

##### `/automod antiraid set`

Raise or lower the antiraid level

- Japanese name: `set`
- Japanese description: アンレイドのレベルを上げ下げする

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `level` | level | string | yes | - | The level to raise or lower the server to |
| `duration` | duration | string | no | - | How long the level lasts, such as 30m or 2h; the calm level comes back after it |

##### `/automod antiraid clear`

Put the antiraid level back to the calm one

- Japanese name: `clear`
- Japanese description: アンレイドのレベルを通常に戻す

##### `/automod antiraid release`

Take a member out of the quarantine

- Japanese name: `release`
- Japanese description: メンバーを隔離から解除する

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | The member to take out of the quarantine |

##### `/automod antiraid release-all`

Take every member out of the quarantine

- Japanese name: `release-all`
- Japanese description: 隔離中のメンバーを全員解除する

## Configuration

Set under `modules.automod.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `antiraid` | object or null | `null` | - | Antiraid: the levels a server is raised to while a raid runs. Absent means antiraid is off. |
| `counters` | object | `{}` | - | The counters, keyed by name: the numbers the rules add to, set, and react to. |
| `dm_on_action` | boolean | `true` | - | Whether the member gets a direct message about a punishment. |
| `dry_run` | boolean | `false` | - | Act on nothing: post what would have happened to the log channel. |
| `remove_roles_on_mute` | any | `false` | - | Which roles a role timeout takes off the member: false, true for every role that can be taken off, or a list of role ids. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `restore_roles_on_mute` | any | `true` | - | Which of the roles taken off come back when the timeout ends: the same forms as remove_roles_on_mute. true by default. |
| `rules` | object | `{}` | - | The rules, keyed by id. Each one is checked on every message; the rules are checked in the order of their ids, and the first one that trips acts unless it says to continue. |
| `timeout_method` | string | `"discord"` | - | How a timeout is given: Discord's timeout, or a role that is taken off when its time is up. |
| `timeout_role` | string or null | `null` | - | The id of the role a timeout gives, when timeout_method is role. |
| `warn_expire_days` | integer | `30` | - | After how many days a warning of a rule expires. |

Default configuration:

```json
{
  "antiraid": null,
  "counters": {},
  "dm_on_action": true,
  "dry_run": false,
  "remove_roles_on_mute": false,
  "restore_roles_on_mute": true,
  "rules": {},
  "timeout_method": "discord",
  "timeout_role": null,
  "warn_expire_days": 30
}
```
