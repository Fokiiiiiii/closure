# Honeypot (`honeypot`)

- Version: 0.1.0
- Enabled by default: no

## Capabilities

- `send_messages`
- `manage_messages`
- `read_guild`
- `kick_members`
- `ban_members`
- `timeout_members`
- `manage_channels`
- `respond_to_interactions`
- `storage`
- `schedule`
- `cases`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `honeypot.manage` | 75 | Set up the honeypot channels |

## Commands

### `/honeypot`

Channels where whoever posts is punished at once

- Japanese name: `ハニーポット`
- Japanese description: 投稿した人をその場で処罰するチャンネル

#### `/honeypot add`

Make a channel a honeypot

- Japanese name: `追加`
- Japanese description: チャンネルをハニーポットにします

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `channel` | チャンネル | channel | yes | - | The channel |

#### `/honeypot remove`

Stop treating a channel as a honeypot

- Japanese name: `解除`
- Japanese description: チャンネルをハニーポットから外します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `channel` | チャンネル | channel | yes | - | The channel |

#### `/honeypot list`

The honeypot channels

- Japanese name: `一覧`
- Japanese description: ハニーポットのチャンネルを表示します

#### `/honeypot create`

Make a channel with a warning in it and set it as a honeypot

- Japanese name: `作成`
- Japanese description: 警告文付きのチャンネルを作り、ハニーポットにします

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `name` | 名前 | string | no | max_length 100 | The name of the new channel; left out, a name of the language of the server |

## Configuration

Set under `modules.honeypot.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `action` | string | `"ban"` | - | What happens to whoever posts in a honeypot channel: ban, kick or timeout. |
| `delete_hours` | integer | `24` | - | How many hours of a banned member's messages are deleted (0 to 168). |
| `exempt_roles` | array | `[]` | role | Roles whose members are never punished. |
| `exempt_users` | array | `[]` | user | Accounts that are never punished. The owner, the bot and anyone with moderator powers are never punished. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |
| `timeout` | string | `"1d"` | - | How long a timeout lasts, such as 1d. |

Default configuration:

```json
{
  "action": "ban",
  "delete_hours": 24,
  "exempt_roles": [],
  "exempt_users": [],
  "timeout": "1d"
}
```
