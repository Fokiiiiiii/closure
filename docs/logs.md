# The logs module

`logs` posts what happens to the messages and members of a server to a channel: edits, deletions
(single and bulk), joins, leaves, bans and unbans. The code is in
[`crates/closure-mod-logs`](../crates/closure-mod-logs/src/lib.rs); the files it posts come from the
history the host keeps, through [`closure_sdk::files`](../crates/closure-sdk/src/files.rs).

## Settings

```yaml
modules:
  logs:
    enabled: true
    config:
      channel: 123456789012345678   # optional; left out, the guild's log channel
      message_edits: true
      message_deletes: true
      member_joins: true
      member_leaves: true
      bans: true
      keep_attachments: false       # see Files below
```

## Files

A message's text is kept by the message history, which is on for a guild while a module there holds
`message_history`. The files of attachments are different: Discord serves an attachment's link only
for about a day, so a link alone cannot show a picture later.

- **Default (`keep_attachments: false`).** Nothing is stored when a message arrives. When a message
  is deleted, or an edit removes an attachment, the host fetches the file then, while Discord may
  still serve it, and the log posts it. A file that cannot be fetched is listed as not kept.
- **`keep_attachments: true`.** The host fetches each new attachment as its message arrives, and
  keeps it. The log then posts the kept file even when Discord no longer serves it. This uses storage:
  the files of a guild are capped at 256 MiB, and the oldest go first. Files are kept for the same
  period as the message history (seven days by default).

Only files up to 8 MiB are fetched or kept. Links are fetched only from Discord's CDN
(`cdn.discordapp.com` and `media.discordapp.net`), over HTTPS and without following redirects.
Each fetch has a short timeout, and a deletion waits for its fetches, so a very large bulk deletion
may keep only part of its files.

## Privacy

Files are user content. Turning `keep_attachments` on keeps them on the bot's database for the
retention period, for every member who posts there. Leave it off unless the server needs the files
after Discord drops them.
