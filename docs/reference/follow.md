# Follow (`follow`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `respond_to_interactions`
- `storage`
- `send_direct_messages`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `follow.use` | 50 | Follow members into voice channels |

## Commands

### `/follow`

Get a direct message when a member joins a voice channel

- Japanese name: `フォロー`
- Japanese description: メンバーがボイスチャンネルに参加したら DM で知らせます

#### `/follow add`

Follow a member

- Japanese name: `追加`
- Japanese description: メンバーをフォローします

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |

#### `/follow remove`

Stop following a member

- Japanese name: `解除`
- Japanese description: メンバーのフォローをやめます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | ユーザー | user | yes | - | Member |

#### `/follow list`

The members you follow

- Japanese name: `一覧`
- Japanese description: フォロー中のメンバーを表示します

## Configuration

Set under `modules.follow.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{}
```
