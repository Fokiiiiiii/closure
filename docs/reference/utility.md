# Utility (`utility`)

Commands that tell about the server, a role, a channel or a member, the names a member went by, and finding or moving members in voice.

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `read_guild`
- `manage_nicknames`
- `respond_to_interactions`
- `storage`
- `move_members`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `utility.info` | 1 | Use the commands that show information |
| `utility.nickname` | 50 | Change the nickname of a member |
| `utility.search` | 50 | Find members by the names they went by |
| `utility.voice` | 50 | Move and disconnect members in voice |

## Commands

### `/serverinfo`

Show facts about this server

- Japanese name: `serverinfo`
- Japanese description: このサーバーの情報を表示します

This command takes no options.

### `/roleinfo`

Show facts about a role

- Japanese name: `roleinfo`
- Japanese description: ロールの情報を表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `role` | role | role | yes | - | The role |

### `/channelinfo`

Show facts about a channel

- Japanese name: `channelinfo`
- Japanese description: チャンネルの情報を表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `channel` | channel | channel | yes | - | The channel |

### `/avatar`

Show the avatar of a member

- Japanese name: `avatar`
- Japanese description: メンバーのアバターを表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | no | - | The member; yourself when left out |

### `/names`

Show the names a member went by

- Japanese name: `names`
- Japanese description: メンバーの過去の名前を表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | The member |

### `/nickname`

Change the nickname of a member, or take it off

- Japanese name: `ニックネーム`
- Japanese description: メンバーのニックネームを変更、または削除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | The member |
| `nickname` | ニックネーム | string | no | max_length 32 | The new nickname; left out, the nickname is taken off |

### `/snowflake`

Tell when a Discord id was made

- Japanese name: `スノーフレーク`
- Japanese description: Discord の ID が作られた日時を調べます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `id` | id | string | yes | max_length 20 | The id of a user, channel, role, message or the like |

### `/findname`

Find the members who went by a name

- Japanese name: `名前検索`
- Japanese description: ある名前で過ごしたことのあるメンバーを探します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `name` | 名前 | string | yes | max_length 64 | A name, exactly as the member had it (case does not matter) |

### `/locate`

Find the voice channel a member is in

- Japanese name: `locate`
- Japanese description: メンバーがいるボイスチャンネルを探します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | The member |

### `/vcmove`

Move a member to another voice channel

- Japanese name: `vcmove`
- Japanese description: メンバーを別のボイスチャンネルへ移動します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | The member, who has to be in voice |
| `channel` | channel | channel | yes | - | The voice channel to move them to |

### `/vcmoveall`

Move everyone in a voice channel to another

- Japanese name: `全員移動`
- Japanese description: ボイスチャンネルにいる全員を別のチャンネルへ移動します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `from` | 移動元 | channel | yes | - | The voice channel to empty |
| `to` | 移動先 | channel | yes | - | The voice channel to move them to |

### `/vckick`

Disconnect a member from voice

- Japanese name: `vckick`
- Japanese description: メンバーをボイスから切断します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | The member |

## Configuration

Set under `modules.utility.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |
| `voice_hop_limit` | integer | `0` | - | How many times a member may go into a voice channel within the window before they are disconnected; 0 sets no limit. |
| `voice_hop_seconds` | integer | `60` | - | The window of the limit on voice channel moves, in seconds. |

Default configuration:

```json
{
  "voice_hop_limit": 0,
  "voice_hop_seconds": 60
}
```
