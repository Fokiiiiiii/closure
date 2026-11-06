# Utility (`utility`)

Commands that tell about the server, a role, a channel or a member, the names a member went by, and finding or moving members in voice.

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `read_guild`
- `respond_to_interactions`
- `storage`
- `move_members`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `utility.info` | 1 | Use the commands that show information |
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

Default configuration:

```json
{}
```
