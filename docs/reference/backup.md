# Backup (`backup`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `read_guild`
- `manage_roles`
- `manage_channels`
- `respond_to_interactions`
- `storage`
- `schedule`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `backup.manage` | 75 | Create, see and restore the backup of the server |

## Commands

### `/backup`

Keep the roles and channels of the server, and bring back what is gone

- Japanese name: `バックアップ`
- Japanese description: サーバーのロールとチャンネルを保存し、消えたものを戻します

#### `/backup create`

Keep the roles and channels now

- Japanese name: `作成`
- Japanese description: いまのロールとチャンネルを保存します

#### `/backup info`

When the backup was taken and what it holds

- Japanese name: `情報`
- Japanese description: バックアップの時刻と内容を表示します

#### `/backup restore`

Create again the roles and channels that the backup holds and the server lacks

- Japanese name: `復元`
- Japanese description: バックアップにあってサーバーに無いロールとチャンネルを作り直します

## Configuration

Set under `modules.backup.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `daily` | boolean | `true` | - | Back up the roles and channels once a day. A backup that would hold less than half of what the last one holds is not taken, so a nuke cannot replace the good backup. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "daily": true
}
```
