# Reminders

`/remind add <when> <text>` schedules a message in the current channel. Use a relative duration such as `10m` or `2d`, from 10 seconds through 30 days. Text is 1–500 characters. A user may have at most 10 pending reminders per server.

`/remind list` shows your pending reminders and their IDs. `/remind cancel <id>` cancels your own reminder. The commands require `remind.use` (general by default). Replies are private by default and follow the module's `replies` setting.

When due, the bot mentions only the creator in the original channel. No DM is sent. A recovery job checks up to 50 reminders every five minutes, restores missing scheduled jobs and retries deliveries at least 90 seconds overdue. A reminder is discarded with a warning once it is more than 24 hours overdue. A 60-second delivery lease prevents concurrent attempts while it remains valid. Delivery is at least once: if the bot stops after sending but before removing the reminder, or a send takes longer than the lease, the message may be sent again.
