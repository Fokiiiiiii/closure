# Moderation (`moderation`)

Warn members, note about them, and look at and revoke the cases of a server.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `read_guild`
- `kick_members`
- `ban_members`
- `timeout_members`
- `mute_members`
- `manage_roles`
- `manage_channels`
- `respond_to_interactions`
- `storage`
- `schedule`
- `cases`
- `send_direct_messages`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `moderation.warn` | 50 | Warn members |
| `moderation.note` | 50 | Add notes about members |
| `moderation.cases` | 50 | See the cases and warnings of members |
| `moderation.revoke` | 50 | Revoke cases |
| `moderation.timeout` | 50 | Time members out and lift timeouts |
| `moderation.kick` | 50 | Kick members |
| `moderation.ban` | 75 | Ban and unban members |
| `moderation.voice` | 50 | Mute members in voice channels and lift it |

## Commands

### `/warn`

Warn a member

- Japanese name: `警告`
- Japanese description: メンバーに警告します。警告は設定した日数で失効します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member to warn |
| `reason` | 理由 | string | no | max_length 512 | Why |

### `/note`

Add a note about a member

- Japanese name: `メモ`
- Japanese description: 警告ではないメモを相手に残します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member the note is about |
| `text` | 内容 | string | yes | max_length 512 | What to note |

### `/warnings`

Show the active warnings of a member

- Japanese name: `警告一覧`
- Japanese description: メンバーの有効な警告を表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |

### `/case`

Look at, list and revoke cases

- Japanese name: `ケース`
- Japanese description: ケースを確認、一覧、取り消しします

#### `/case view`

Show a case

- Japanese name: `表示`
- Japanese description: ケースを表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | Number of the case |

#### `/case list`

List the cases of a member

- Japanese name: `一覧`
- Japanese description: メンバーのケースを新しい順に一覧表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |
| `before` | 前の番号 | integer | no | - | Show only the cases numbered below this |

#### `/case revoke`

Revoke a case

- Japanese name: `取り消し`
- Japanese description: ケースを取り消します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | Number of the case |
| `reason` | 理由 | string | no | max_length 512 | Why it is revoked |

### `/timeout`

Time a member out

- Japanese name: `タイムアウト`
- Japanese description: メンバーを一定時間タイムアウトします。Discord の上限は 28 日です

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member to time out |
| `duration` | 期間 | string | no | - | How long, such as 1h or 7d, or until a date and time; at most 28 days. A template may give it. |
| `reason` | 理由 | string | no | max_length 512 | Why |

### `/untimeout`

Lift the timeout of a member

- Japanese name: `タイムアウト解除`
- Japanese description: メンバーのタイムアウトを解除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |
| `reason` | 理由 | string | no | max_length 512 | Why |

### `/kick`

Kick a member

- Japanese name: `キック`
- Japanese description: メンバーをサーバーから追い出します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |
| `reason` | 理由 | string | no | max_length 512 | Why |

### `/ban`

Ban a member, for good or for a while

- Japanese name: `バン`
- Japanese description: メンバーを BAN します。期間を付ければ一時的な BAN になります

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member to ban |
| `reason` | 理由 | string | no | max_length 512 | Why |
| `duration` | 期間 | string | no | - | How long the ban lasts, such as 7d, or until a date and time; forever when left out |
| `delete_days` | 削除日数 | integer | no | min 0; max 7 | How many days of the member's messages to delete, 0 to 7 |

### `/unban`

Lift the ban of a member

- Japanese name: `バン解除`
- Japanese description: メンバーの BAN を解除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |
| `reason` | 理由 | string | no | max_length 512 | Why |

### `/vcmute`

Mute a member in voice channels

- Japanese name: `vcミュート`
- Japanese description: メンバーをボイスチャンネルでミュートします。期間を付けなければ解除するまで続きます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member to mute in voice channels |
| `duration` | 期間 | string | no | - | How long, such as 1h or 7d, or until a date and time; until lifted when left out |
| `reason` | 理由 | string | no | max_length 512 | Why |

### `/vcunmute`

Lift the voice mute of a member

- Japanese name: `vcミュート解除`
- Japanese description: メンバーのボイスミュートを解除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |
| `reason` | 理由 | string | no | max_length 512 | Why |

## Configuration

Set under `modules.moderation.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `ban_delete_days` | integer | `0` | - | How many days of the messages of a banned member are deleted. |
| `dm_on_action` | boolean | `true` | - | Whether the warned member gets a direct message. |
| `escalation` | array | `[]` | - | What a warning brings about when a member reaches a number of active warnings. |
| `remove_roles_on_mute` | any | `false` | - | Which roles a role timeout or voice mute takes off the member: false, true for every role that can be taken off, or a list of role ids. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `require_reason` | boolean | `false` | - | Whether a warning needs a reason. |
| `restore_roles_on_mute` | any | `true` | - | Which of the roles taken off come back when the punishment ends: the same forms as remove_roles_on_mute. true by default. |
| `templates` | object | `{}` | - | Templates a moderator picks from when a command leaves out the reason or the duration. |
| `texts` | any | - | - | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |
| `timeout_method` | string | `"discord"` | - | How a timeout is given: Discord's timeout, or a role that is taken off when its time is up. |
| `timeout_role` | string or null | `null` | role | The id of the role a timeout gives, when timeout_method is role. |
| `voice_mute_method` | string | `"discord"` | - | How a voice mute is given: Discord's voice mute, or a role that is taken off when its time is up. |
| `voice_mute_role` | string or null | `null` | role | The id of the role a voice mute gives, when voice_mute_method is role. |
| `warn_expire_days` | integer | `30` | - | After how many days a warning expires. |

Default configuration:

```json
{
  "ban_delete_days": 0,
  "dm_on_action": true,
  "escalation": [],
  "remove_roles_on_mute": false,
  "require_reason": false,
  "restore_roles_on_mute": true,
  "templates": {},
  "timeout_method": "discord",
  "timeout_role": null,
  "voice_mute_method": "discord",
  "voice_mute_role": null,
  "warn_expire_days": 30
}
```
