# Channels

The `channels` module is disabled by default. Enable it and configure channel IDs as strings:

```yaml
modules:
  channels:
    enabled: true
    config:
      auto_delete:
        - channel: "123456789012345678"
          after: 1h
          skip_pinned: true
      auto_react:
        - channel: "234567890123456789"
          emojis: ["👍", "<:example:345678901234567890>"]
          skip_bots: true
```

Each list accepts at most 25 channels. The delay is 5 seconds to 7 days; use durations such as
`30s`, `1h30m`, or `7d`. Each reaction rule takes 1–5 Unicode or custom emojis. Both skip options
default to true. A duplicate channel uses its first rule and logs a warning. Deletion jobs survive
restarts and never delete messages in log delivery channels, including configured channels,
webhook channels and fallbacks. The destinations are checked again when a job runs. If a named
webhook cannot be resolved, automatic deletion and reactions are suspended for safety. Discord
permissions are required to read and delete messages and add reactions.
