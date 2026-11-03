# Tags (`tags`)

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `send_messages`
- `respond_to_interactions`
- `storage`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `tags.use` | 1 | Use saved messages |
| `tags.manage` | 50 | Add or remove saved messages |

## Commands

### `/tag`

Manage reusable messages

- Japanese name: `tag`
- Japanese description: 定型文を管理します

#### `/tag show`

Post a saved message

- Japanese name: `show`
- Japanese description: 保存した定型文を投稿します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `name` | name | string | yes | max_length 32 | Name of the message |

#### `/tag add`

Save a message

- Japanese name: `add`
- Japanese description: 定型文を保存します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `name` | name | string | yes | max_length 32 | Name of the message |
| `text` | text | string | yes | min_length 1; max_length 2000 | Message to save |

#### `/tag remove`

Remove a saved message

- Japanese name: `remove`
- Japanese description: 定型文を削除します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `name` | name | string | yes | max_length 32 | Name of the message |

#### `/tag list`

List saved messages

- Japanese name: `list`
- Japanese description: 保存した定型文を一覧表示します

## Configuration

Set under `modules.tags.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `texts` | any | - | - | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{}
```
