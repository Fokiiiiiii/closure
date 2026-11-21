# Moderation (`moderation`)

Warn members, note about them, and look at and revoke the cases of a server.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `manage_messages`
- `read_guild`
- `read_bans`
- `kick_members`
- `ban_members`
- `timeout_members`
- `mute_members`
- `manage_roles`
- `manage_channels`
- `read_audit_log`
- `respond_to_interactions`
- `storage`
- `schedule`
- `cases`
- `send_direct_messages`
- `read_messages`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `moderation.userinfo` | 50 | View user information and cases. |
| `moderation.massban` | 100 | Ban multiple users. |
| `moderation.warn` | 50 | Warn members |
| `moderation.note` | 50 | Add notes about members |
| `moderation.cases` | 50 | See the cases and warnings of members |
| `moderation.revoke` | 50 | Revoke cases |
| `moderation.addcase` | 50 | Record and change cases by hand |
| `moderation.hidecase` | 75 | Hide cases from the lists |
| `moderation.deletecase` | 75 | Delete cases for good |
| `moderation.appeal` | 1 | Appeal a punishment |
| `moderation.mywarnings` | 1 | See your own warnings |
| `moderation.appeals` | 50 | Decide on appeals and file them for others |
| `moderation.approve` | 75 | Approve what another moderator asked for |
| `moderation.export` | 75 | Export cases as a file |
| `moderation.timeout` | 50 | Time members out and lift timeouts |
| `moderation.kick` | 50 | Kick members |
| `moderation.ban` | 75 | Ban and unban members |
| `moderation.voice` | 50 | Mute members in voice channels and lift it |
| `moderation.clear` | 50 | Delete the newest messages of a channel |
| `moderation.slowmode` | 50 | Change channel slowmode. |
| `moderation.lock` | 50 | Lock and unlock channels. |

## Commands

### `/warn`

Warn a member; it expires after a set time

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

### `/mywarnings`

Show your own active warnings

- Japanese name: `自分の警告`
- Japanese description: 自分の有効な警告を表示します

This command takes no options.

### `/clear`

Delete the newest messages of the channel

- Japanese name: `一括削除`
- Japanese description: チャンネルの新しいメッセージを消す

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `count` | 件数 | integer | yes | - | How many messages, from 1 to 100 |
| `user` | user | user | no | - | Only the messages of this member |
| `bots` | bots | boolean | no | - | Yes: only the messages of bots |
| `contains` | contains | string | no | max_length 100 | Only the messages that contain this text |
| `links` | links | boolean | no | - | Yes: only messages with links or invites |
| `files` | files | boolean | no | - | Yes: only the messages with a file |
| `pinned` | pinned | boolean | no | - | Yes: pinned messages are deleted too |

### `/bulk`

Punish many members after a preview

- Japanese name: `一括処罰`
- Japanese description: 確認してから、多数のメンバーをまとめて処罰します

#### `/bulk start`

Preview a bulk ban, kick, timeout or unban

- Japanese name: `開始`
- Japanese description: 一括処罰の内容を確認してから実行する

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `action` | 処理 | string | yes | max_length 16 | ban, kick, timeout or unban |
| `users` | ユーザー | string | yes | - | User IDs or mentions, separated by spaces |
| `reason` | 理由 | string | yes | max_length 512 | Why |
| `duration` | 期間 | string | no | max_length 32 | Timeout length, e.g. 1h or 3d; bans are permanent |
| `delete_days` | 削除日数 | integer | no | min 0; max 7 | Days of messages a ban deletes, 0 to 7 |

#### `/bulk status`

Where a bulk operation stands

- Japanese name: `状況`
- Japanese description: 一括処罰の状況を見ます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | The number of the operation |

#### `/bulk cancel`

Stop a bulk operation between batches

- Japanese name: `中止`
- Japanese description: 一括処罰をバッチの間で止めます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | The number of the operation |

#### `/bulk retry`

Take the members that failed again

- Japanese name: `再試行`
- Japanese description: 失敗したメンバーをもう一度処理します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | The number of the operation |

### `/case`

Look at, list and revoke cases

- Japanese name: `ケース`
- Japanese description: ケースを確認、一覧、取り消しします

#### `/case view`

Show a case or a member's cases

- Japanese name: `表示`
- Japanese description: ケースを表示、またはメンバーのケースを一覧します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | no | - | Number of the case, to see one |
| `user` | ユーザー | user | no | - | Member, to list their cases |
| `before` | 前の番号 | integer | no | - | With a member: only cases below this number |

#### `/case revoke`

Revoke a case

- Japanese name: `取り消し`
- Japanese description: ケースを取り消します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | Number of the case |
| `reason` | 理由 | string | no | max_length 512 | Why it is revoked |

#### `/case add`

Record a case by hand

- Japanese name: `追加`
- Japanese description: ケースを手で記録する

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |
| `kind` | 種類 | string | yes | - | note, warn, kick, ban, timeout, softban, ... |
| `reason` | 理由 | string | no | max_length 512 | Why |

#### `/case update`

Change a case's reason or moderator

- Japanese name: `更新`
- Japanese description: ケースの理由や、実行者として示す人を変えます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | Number of the case |
| `reason` | 理由 | string | no | max_length 512 | The new reason |
| `moderator` | 実行者 | user | no | - | The moderator to name for the case |

#### `/case hide`

Hide a case, or show it again

- Japanese name: `非表示`
- Japanese description: ケースを一覧から隠す、または再表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | Number of the case |
| `show` | 再表示 | boolean | no | - | Yes: show the case again |

#### `/case delete`

Delete a case for good

- Japanese name: `削除`
- Japanese description: ケースを完全に削除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `number` | 番号 | integer | yes | - | Number of the case |
| `confirm` | 確認 | boolean | yes | - | Yes: the case is gone for good |

#### `/case search`

Find cases by member, kind, state, time or reason

- Japanese name: `検索`
- Japanese description: メンバー・種類・状態・期間・理由でケースを探す

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | no | - | Cases against this member |
| `moderator` | 実行者 | user | no | - | Cases made by this moderator |
| `kind` | 種類 | string | no | max_length 32 | Kind: warn, note, ban, kick, timeout, ... |
| `source` | 発生元 | string | no | max_length 16 | Source: command, button, automod, manual, ... |
| `state` | 状態 | string | no | max_length 16 | State: active, expired or revoked |
| `within` | 期間 | string | no | max_length 32 | Made within this long, such as 7d |
| `text` | 語 | string | no | max_length 100 | A word of the reason |
| `hidden` | 非表示も含める | boolean | no | - | Include hidden cases (needs permission) |
| `before` | 前の番号 | integer | no | - | Show only the cases numbered below this |
| `export` | 出力 | boolean | no | - | Yes: send a CSV to the export channel |

### `/slowmode`

Set slowmode; over 6 hours the bot enforces it

- Japanese name: `slowmode`
- Japanese description: 低速モードを設定（6 時間超は Bot が削除で実現）

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `seconds` | seconds | integer | yes | min 0; max 2592000 | Seconds from 0 to 21600 |
| `channel` | channel | channel | no | - | Channel to change |
| `duration` | duration | string | no | - | Time before restoring the previous value |

### `/lock`

Lock a channel

- Japanese name: `lock`
- Japanese description: チャンネルをロック

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `channel` | channel | channel | no | - | Channel to lock |
| `duration` | duration | string | no | - | Time before permissions are restored |
| `reason` | reason | string | no | max_length 512 | Why |

### `/unlock`

Restore a locked channel

- Japanese name: `unlock`
- Japanese description: チャンネルのロックを解除

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `channel` | channel | channel | no | - | Channel to unlock |

### `/userinfo`

Show a user's membership and cases

- Japanese name: `userinfo`
- Japanese description: ユーザーの参加情報とケース履歴を表示

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | User to inspect |

### `/timeout`

Time out a member, up to 28 days

- Japanese name: `タイムアウト`
- Japanese description: メンバーをタイムアウト（最長 28 日）

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member to time out |
| `duration` | 期間 | string | no | - | How long (1h, 7d) or until (2026-11-01 18:00) |
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

Kick a member from the server

- Japanese name: `キック`
- Japanese description: メンバーをサーバーから追い出します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member to kick |
| `reason` | 理由 | string | no | max_length 512 | Why |

### `/ban`

Ban a member, for good or for a while

- Japanese name: `バン`
- Japanese description: メンバーを BAN（期間で一時 BAN）

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member to ban |
| `reason` | 理由 | string | no | max_length 512 | Why |
| `duration` | 期間 | string | no | - | How long (7d) or until a date; forever if left out |
| `delete_days` | 削除日数 | integer | no | min 0; max 7 | Days of messages to delete, 0 to 7 |
| `soft` | ソフト | boolean | no | - | Yes: softban; the ban is lifted at once |

### `/appeal`

Ask for a punishment to be lifted

- Japanese name: `異議`
- Japanese description: 処罰の解除を求めます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `text` | 理由 | string | yes | max_length 400 | Why the punishment should be lifted |
| `user` | ユーザー | user | no | - | The member, for appeals that came another way |

### `/unban`

Lift the ban of a member

- Japanese name: `バン解除`
- Japanese description: メンバーの BAN を解除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |
| `reason` | 理由 | string | no | max_length 512 | Why |

### `/vcmute`

Mute a member in voice

- Japanese name: `vcミュート`
- Japanese description: メンバーをボイスでミュート

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member to mute in voice channels |
| `duration` | 期間 | string | no | - | How long (1h, 7d) or until a date; until lifted |
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
| `alert_channel` | string or null | `null` | channel | The staff channel of the alert on a rejoin. |
| `alert_on_rejoin` | boolean | `false` | - | Tell the staff channel when a member who has cases comes back to the server. |
| `approval_bulk_from` | integer | `0` | - | A bulk operation of this many members or more needs another moderator to approve it (0 turns this off). |
| `approval_channel` | string or null | `null` | channel | The staff channel where requests for approval are posted. |
| `approval_permanent_ban` | boolean | `false` | - | A ban for good by someone who may not approve needs another moderator to approve it. |
| `ban_delete_days` | integer | `0` | - | How many days of the messages of a banned member are deleted. |
| `dm_messages` | object | `{}` | - | Custom direct-message templates by punishment kind. |
| `dm_on_action` | boolean | `true` | - | Whether the warned member gets a direct message. |
| `escalation` | array | `[{"action":"timeout","duration":"1d","warns":3},{"action":"kick","warns":5}]` | - | What a warning brings about when a member reaches a number of active warnings. |
| `escalation_enabled` | boolean | `true` | - | Whether a warning can bring about a punishment. Off keeps the rules. |
| `export_channel` | string or null | `null` | channel | The staff channel the exports of cases (CSV files) are sent to. |
| `manual_cases` | boolean | `true` | - | Record a ban, a kick or a timeout made by hand in Discord as a case, with the moderator the audit log names. |
| `remove_roles_on_mute` | any | `false` | flag-or-roles | Which roles a role timeout or voice mute takes off the member: false, true for every role that can be taken off, or a list of role ids. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `require_reason` | boolean | `false` | - | Whether a warning needs a reason. |
| `restore_roles_on_mute` | any | `true` | flag-or-roles | Which of the roles taken off come back when the punishment ends: the same forms as remove_roles_on_mute. true by default. |
| `slowmode_exempt_roles` | array | `[]` | - | The roles that a slowmode kept by the bot does not hold back. |
| `templates` | object | `{}` | - | Templates a moderator picks from when a command leaves out the reason or the duration. |
| `templates_enabled` | boolean | `true` | - | Whether the templates are offered. Off keeps them. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |
| `timeout_method` | string | `"discord"` | - | How a timeout is given: Discord's timeout, or a role that is taken off when its time is up. |
| `timeout_role` | string or null | `null` | role | The id of the role a timeout gives, when timeout_method is role. |
| `voice_mute_method` | string | `"discord"` | - | How a voice mute is given: Discord's voice mute, or a role that is taken off when its time is up. |
| `voice_mute_role` | string or null | `null` | role | The id of the role a voice mute gives, when voice_mute_method is role. |
| `warn_expire_days` | integer | `30` | - | After how many days a warning expires. |

Default configuration:

```json
{
  "alert_channel": null,
  "alert_on_rejoin": false,
  "approval_bulk_from": 0,
  "approval_channel": null,
  "approval_permanent_ban": false,
  "ban_delete_days": 0,
  "dm_messages": {},
  "dm_on_action": true,
  "escalation": [
    {
      "action": "timeout",
      "duration": "1d",
      "warns": 3
    },
    {
      "action": "kick",
      "warns": 5
    }
  ],
  "escalation_enabled": true,
  "export_channel": null,
  "manual_cases": true,
  "remove_roles_on_mute": false,
  "require_reason": false,
  "restore_roles_on_mute": true,
  "slowmode_exempt_roles": [],
  "templates": {},
  "templates_enabled": true,
  "timeout_method": "discord",
  "timeout_role": null,
  "voice_mute_method": "discord",
  "voice_mute_role": null,
  "warn_expire_days": 30
}
```
