# Welcome module

The `welcome` module greets members when they join. It is disabled by default. Set
`modules.welcome.enabled: true` and configure a channel before it sends anything.

## Configuration

```yaml
modules:
  welcome:
    enabled: true
    channel: "123456789012345678"
    # message: "Welcome {user} (@{username}) to {server}! You are member {count}."
    # delete_after: 1h
    # include_bots: false
```

- `channel`: Discord channel snowflake as a string. If absent, no greeting is sent.
- `message`: optional template. Without one, the module uses the server language's catalog default.
- `delete_after`: optional duration from 5 seconds through 7 days.
- `include_bots`: defaults to `false`.

Templates support `{user}` (the joining member's mention), `{username}`, `{server}`, and
`{count}`. Only the joining member is allowed to be mentioned. The server details are read only
when the template uses `{server}` or `{count}`. If Discord does not return the member count,
`{count}` renders empty. The module sends at most five greetings per server in any 60-second
window; additional joins are skipped and logged at debug level.

When `delete_after` is set, a scheduled job deletes each successfully sent greeting. Deletion
requires the bot to have permission to manage messages in that channel.
