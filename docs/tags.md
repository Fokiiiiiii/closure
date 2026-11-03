# Tags

The tags module stores up to 100 reusable messages per server.

## Commands

- `/tag show <name>` posts the saved message. The name option offers autocomplete suggestions.
- `/tag add <name> <text>` saves a message.
- `/tag remove <name>` removes a message.
- `/tag list` lists saved names.

Names are lowercased and must match `[a-z0-9_-]{1,32}`. Message text must contain 1 to 2,000 characters. Adding an existing name fails; remove it before adding it again.

## Permissions

- `tags.use` defaults to `general` and is required by `show` and `list`.
- `tags.manage` defaults to `moderator` and is required by `add` and `remove`.

## Storage and replies

Messages are stored in the module's guild-scoped KV. A tag shown by `show` is posted as a normal public reply, subject to the server's `replies` setting. Mentions in saved text are disabled.
