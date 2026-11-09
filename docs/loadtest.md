

## After gathering the log entries

The logs module now posts a server's first six entries a minute as they come and gathers the rest
into batches (ten embeds to a message; a batch that is not full goes out five seconds after its
first entry). Nothing is left out, it only comes a little later and together.

| | Before | After |
| --- | --- | --- |
| Joins only, requests to Discord | 5,985 (3.0 a join) | 4,211 (2.1 a join) |
| of which `send_message` | 2,025 | 250 |
| Full run, `send_message` | 1,435 | 252 |
| Ordinary chat under the limit of 50 a second, shed | 825 (10-24%) | 0 |

The raid and the spam phases still shed about 99% at full rate: the load is some two hundred times
the budget. The calm and mildly busy servers are what this changes.

## What this test does not show

- It is a load of synthetic events over an id space of the size named (`LOAD_MEMBERS`), not that
  many members kept in the cache and the database. The memory of that many members, the scans of
  all of them and the share of protections that succeeded are not measured here.
- The scheduler is given to the modules but its runner is not started: the jobs they book (the
  five-second post of a part-full log batch, the recovery of a punishment, the timed lifts) are not
  run, so the count of requests leaves them out and the delivery of every log entry is not checked.
- Every action of the SDK costs one slot of the limit. On Discord some actions finish in the
  cache and some are several requests, and the limits of single routes, the interactions that the
  limit does not count and the invalid requests are not reproduced. The numbers show the shape of
  the load, not the speed a real server would see at 50 requests a second.
- `accepted` counts the events the dispatcher took in, not the ones that were handled to the end.
