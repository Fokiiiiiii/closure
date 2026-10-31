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
      channels: true                # channel created, changed and deleted
      threads: true                 # thread created, archived, unarchived and deleted
      roles: true                   # role created, changed and deleted
      member_roles: true            # roles given to or taken from a member
      member_nicknames: true        # nickname changed
      member_timeouts: true         # member timed out, and the timeout lifted
      voice: false                  # joins, leaves and moves of voice channels
      emojis: true                  # emoji created, deleted and changed
      stickers: true                # sticker created, deleted and changed
      stage_instances: true         # stage started, changed and ended
      keep_attachments: false       # see Files below
```

## What an entry shows

- **Join.** The member, when the account was made and how old it is, and when the member joined.
- **Edit.** The text before the edit and after it, the message's id and when it was posted, and the
  files the edit took away. The text before an edit is the text the history held when the edit
  arrived, so the text an edit replaced is shown but the one before it is not. Without the history
  (its message history is off for the guild) the text before says that it was not kept.
- **Deletion.** The text, the files and the id of the message, and when it was posted. The entry is
  stamped with the time of the deletion.
- **Channels and threads.** A change shows what changed: the name, the category, the topic and NSFW.
  Creations, archives and deletions are shown as they happen.
- **Roles.** A change shows what changed: the name, the colour (as a hex code), the position and the
  flags. A role the bot did not see before is not compared.
- **Members.** Roles given and taken, the nickname before and after, and a Discord timeout that is set
  or lifted. These compare with what the bot knew about the member before, so a member the bot did
  not see is not compared.
- **Voice.** A join, a leave or a move of a voice channel, against the channel the bot knew the member
  to be in. Off by default: it is busy on a large server.
- **Emojis, stickers and stages.** Created, deleted and changed. A change compares with the list the
  bot knew before the update.
- **Text in code blocks.** Message text, topics, nicknames and sticker descriptions are shown in code
  blocks, so they read as written and cannot format the entry around them.
- **Who made a change.** Channels, roles, a member's roles, nickname and timeout, emojis, stickers, stages,
  threads, bans, unbans, and a voice disconnect or move show who made the change and the reason they gave,
  from the audit log. A kick shows as a kick, not as a departure. The audit log is read for the newest
  entry of the same kind that names the same target and is less than a minute old; that minute is an
  assumption, not measured. A change the member made to themselves, or one Discord made, shows no name.
  A deleted message shows who deleted it when the audit log has a deletion of a message by the same author
  in the same channel, within the minute; a message its author deleted shows no name. The audit log does
  not say which message was deleted, so a moderator's deletion of another message by the same author in
  the same channel within the minute can be named for the author's own deletion; and the message history
  must hold the message to know its author. A bulk delete shows who made it when the audit log has an
  entry for the same channel and the same number of messages. Reading the
  audit log needs the bot to view it: without that, entries are still posted, without who made the change,
  and the failure is logged.

## Files

A message's text is kept by the message history, which is on for a guild while a module there holds
`message_history`. The files of attachments are different: Discord serves an attachment's link only
for about a day, so a link alone cannot show a picture later.

- **Default (`keep_attachments: false`).** Nothing is stored when a message arrives. When a message
  is deleted, or an edit removes an attachment, the host fetches the file then, while Discord may
  still serve it, and the log posts it. A file that cannot be fetched is listed as not kept.
- **`keep_attachments: true`.** The host fetches each new attachment as its message arrives, and
  keeps it. At most 64 arrivals wait for their files at once; an arrival past that is not kept as it
  arrives, and its files are fetched only when a deletion or an edit needs them. The log then posts
  the kept file even when Discord no longer serves it. This uses storage:
  the files of a guild are capped at 256 MiB, and the oldest go first. Files are kept for the same
  period as the message history (seven days by default).

A picture (png, jpg, jpeg, gif or webp) is shown inside its entry's embed; any other file is posted
with the entry as an attachment.

Only files up to 8 MiB are fetched or kept. Links are fetched only from Discord's CDN
(`cdn.discordapp.com` and `media.discordapp.net`), over HTTPS and without following redirects.
Each fetch has a short timeout, and a deletion waits for its fetches, so a very large bulk deletion
may keep only part of its files.

## Privacy

Files are user content. Turning `keep_attachments` on keeps them on the bot's database for the
retention period, for every member who posts there. Leave it off unless the server needs the files
after Discord drops them.
