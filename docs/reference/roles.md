# Roles (`roles`)

Roles that members take themselves, and roles that moderators hand out.

- Version: 0.1.0
- Enabled by default: yes

## Capabilities

- `send_messages`
- `read_guild`
- `manage_roles`
- `respond_to_interactions`
- `schedule`

## Permissions

| Key | Default level | Description |
|---|---|---|
| `roles.self` | 1 | Take and leave the roles the server listed |
| `roles.manage` | 50 | Give roles to members and take them back |
| `roles.ping` | 25 | Let a pingable role be pinged for a minute |

## Commands

### `/role`

Take, leave and hand out roles

- Japanese name: `role`
- Japanese description: ロールを取る・外す・付ける

#### `/role join`

Take a role from the list

- Japanese name: `join`
- Japanese description: 一覧にあるロールを取ります

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `role` | role | role | yes | - | The role |

#### `/role leave`

Leave a role you took

- Japanese name: `leave`
- Japanese description: 取ったロールを外します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `role` | role | role | yes | - | The role |

#### `/role list`

Show the roles you can take

- Japanese name: `list`
- Japanese description: 取れるロールを表示します

#### `/role give`

Give a role to a member

- Japanese name: `give`
- Japanese description: メンバーにロールを付けます

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | The member |
| `role` | role | role | yes | - | The role |

#### `/role take`

Take a role from a member

- Japanese name: `take`
- Japanese description: メンバーからロールを外します

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `user` | user | user | yes | - | The member |
| `role` | role | role | yes | - | The role |

### `/pingrole`

Let a role be pinged for a minute

- Japanese name: `pingrole`
- Japanese description: ロールを1分間だけメンションできるようにします

| Option | Japanese name | Type | Required | Limits | Description |
|---|---|---|---|---|---|
| `role` | role | role | yes | - | The role |

## Configuration

Set under `modules.roles.config` in a server's configuration; see [configuration](../configuration.md).

| Key | Type | Default | Widget | Description |
|---|---|---|---|---|
| `pingable_roles` | array | `[]` | role | Roles that can be pinged for a minute with /pingrole. They are not pingable otherwise. |
| `reaction_roles` | array | `[]` | - | Reactions that give a role: the message, the emoji and the role. |
| `replies` | string | - | - | Who sees the replies of the module's commands: "private" only the user who used the command, "public" everyone in the channel. Unset: as the module chose. |
| `self_roles` | array | `[]` | role | The roles members may take for themselves, with a command, a button or a reaction. |
| `texts` | any | - | entries | Replaces the module's texts by message id; {name} is filled in with the argument name. An empty or non-text value keeps the default. |

Default configuration:

```json
{
  "pingable_roles": [],
  "reaction_roles": [],
  "self_roles": []
}
```
