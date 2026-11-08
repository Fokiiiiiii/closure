# Load test

`crates/closure/tests/load.rs` runs the real dispatcher, every shipped module and a real PostgreSQL
against a pretend Discord. Several large servers take ordinary chat, then a raid (a flood of joins
with invite, scam, mention and repeated-line spam from a few hundred accounts), then spam that goes
on. The servers run the heat example from [`examples/heat.yaml`](examples/heat.yaml) with a few
more rules. It prints, per phase, what was sent, accepted and shed, how large the backlog got and
how long it took to clear; then what the modules did and which rows were written.

It makes its own database (`closure_load_<pid>`) and drops it at the end. It does not touch the
running bot, its database or Discord.

```text
DATABASE_URL='postgres:///postgres?host=/tmp&user=chaos' \
  cargo test -p closure --release --test load -- --ignored --nocapture
```

| Variable | Default | Meaning |
| --- | --- | --- |
| `LOAD_GUILDS` | 5 | Servers |
| `LOAD_MEMBERS` | 300000 | Distinct accounts that talk in each server |
| `LOAD_SCALE` | 1.0 | A factor on every rate |
| `LOAD_SECONDS` | 1.0 | A factor on every phase length (15 s, 10 s, 15 s) |
| `LOAD_API_RPS` | 50 | Discord's limit for the whole bot, requests a second; 0 switches it off |
| `LOAD_API_MS` | 60 | How long Discord takes to answer one request |
| `LOAD_DRAIN` | 60 | How long to wait for the backlog to clear after a phase, seconds |

What it does not model: the gateway itself, the member cache, the file keeping of attachments, the
per-route limits of Discord (only the whole-bot limit and a fixed answer time), and the history hook
(the messages are saved one by one with at most 48 writes at a time).

## What it found (2026-11-08)

Per phase and per server: chat 100 messages a second throughout; the raid adds 2,000 joins and 400
spam messages; the spam phase adds 1,500 spam messages. Five servers.

| Run | Phase | Sent | Accepted | Shed |
| --- | --- | --- | --- | --- |
| Discord unlimited | chat | 7,880 | 7,880 | 0 |
| | raid | 125,125 | 25,831 | 99,294 (79%) |
| | spam | 121,580 | 117,705 | 3,875 (3%) |
| Discord at 50 requests a second | chat | 7,880 | 5,995 | 1,885 (24%) |
| | raid | 125,125 | 1,397 | 123,728 (99%) |
| | spam | 121,580 | 1,844 | 119,736 (98%) |
| same, rates at 10% | chat | 785 | 785 | 0 |
| | raid | 12,510 | 1,410 | 11,100 (89%) |
| | spam | 12,155 | 1,433 | 10,722 (88%) |

- Without Discord's limit the modules and the database keep up (the backlog cleared in under a
  second after every phase; memory stayed under 100 MB). The raid still sheds 79%: a server queue
  holds 256 events, and joins cost the most.
- With the whole-bot limit of 50 requests a second, the number of requests is the bottleneck. A
  join costs about five to eight of them: the quarantine of a joiner lists the roles, reads the
  member again and gives the role; the welcome and the log entry each send a message. At 50 a second
  that is a few joins a second for the whole bot.
- The reads (`get_member`, `list_roles`, `get_guild`) go to Discord every time: the cache only
  answers the owner of a server, the server of a channel and the role hierarchy check.
- What is shed is not chosen: a join, which the heat counter and the join-burst rule need, is shed
  as readily as a chat message.
