

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

