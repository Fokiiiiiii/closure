# Remind (`remind`)

Schedule reminders in this channel.

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `send_messages`
- `respond_to_interactions`
- `storage`
- `schedule`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `remind.use` | 1 |  |

## Commands

### `/remind`

Manage your reminders

- Japanese name: `remind`
- Japanese description: リマインダーを管理

#### `/remind add`

Schedule a reminder

- Japanese name: `add`
- Japanese description: リマインダーを予約

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `when` | when | string | yes | - | Relative time from 10 seconds to 30 days |
| `text` | text | string | yes | min_length 1; max_length 500 | Reminder text, up to 500 characters |

#### `/remind list`

List your pending reminders

- Japanese name: `list`
- Japanese description: 未実行のリマインダーを表示

#### `/remind cancel`

Cancel a reminder

- Japanese name: `cancel`
- Japanese description: リマインダーを取り消す

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `id` | id | string | yes | - | Reminder ID |

## Configuration

Set under `modules.remind.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | - | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{}
```
