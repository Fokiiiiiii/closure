# Utility (`utility`)

Commands that tell about the server, a role, a channel or a member, the names a member went by, and finding or moving members in voice.

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `read_guild`
- `manage_nicknames`
- `respond_to_interactions`
- `storage`
- `read_messages`
- `move_members`
- `manage_invites`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `utility.info` | 1 | Use the commands that show information |
| `utility.nickname` | 50 | Change the nickname of a member |
| `utility.search` | 50 | Find members by the names they went by |
| `utility.invites` | 75 | Delete invites that were hardly used |
| `utility.voice` | 50 | Move and disconnect members in voice |

## Commands

### `/info`

Show a role, channel, invite, message, emoji or server

- Japanese name: `info`
- Japanese description: ロール・チャンネル・招待・メッセージなどの情報

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `role` | role | role | no | - | A role to tell about |
| `channel` | channel | channel | no | - | A channel to tell about |
| `invite` | invite | string | no | max_length 120 | An invite link or code to inspect |
| `message` | message | string | no | max_length 140 | A message link of this server |
| `emoji` | emoji | string | no | max_length 80 | An emoji, or its id |

### `/avatar`

Show the avatar of a member

- Japanese name: `avatar`
- Japanese description: メンバーのアバターを表示します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | no | - | The member; yourself when left out |

### `/names`

Show a member's past names, or search by name

- Japanese name: `names`
- Japanese description: 過去の名前を表示、または名前で探す

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | no | - | The member; yourself when left out |
| `name` | name | string | no | max_length 64 | Instead: find members who used this name |

### `/nickname`

Change or remove a member's nickname

- Japanese name: `ニックネーム`
- Japanese description: メンバーのニックネームを変更、または削除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | The member |
| `nickname` | ニックネーム | string | no | max_length 32 | New nickname; removed if left out |

### `/snowflake`

Tell when a Discord id was made

- Japanese name: `スノーフレーク`
- Japanese description: Discord の ID が作られた日時を調べます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `id` | id | string | yes | max_length 20 | An ID of a user, channel, role or message |

### `/inviteprune`

Delete rarely used invites

- Japanese name: `招待整理`
- Japanese description: 使用回数の少ない招待を削除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `max_uses` | 使用回数 | integer | no | - | Delete invites used this often or less; default 1 |

### `/locate`

Find the voice channel a member is in

- Japanese name: `locate`
- Japanese description: メンバーがいるボイスチャンネルを探します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | The member |

### `/vcmove`

Move a member or a voice channel

- Japanese name: `vcmove`
- Japanese description: メンバーやチャンネルを移動する

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | no | - | The member, who has to be in voice |
| `channel` | channel | channel | no | - | Destination; omit to disconnect one member |
| `from` | from | channel | no | - | Instead: the voice channel to empty |

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
