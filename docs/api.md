# HTTP API

`GET /healthz` and `GET /readyz` are unauthenticated. All other endpoints require
`Authorization: Bearer <admin token>`. Keep the token on the server: a dashboard BFF must never
send it to a browser. The BFF authenticates users with Discord OAuth2 and checks their access
using the endpoint below before allowing dashboard actions.

| Method and path | Purpose |
| --- | --- |
| GET `/healthz` | Liveness |
| GET `/readyz` | Gateway readiness |
| GET `/metrics` | Metrics |
| GET `/v1/schema` | Configuration schema; optional `view` and `locale` |
| GET `/v1/guilds` | Joined servers |
| GET `/v1/guilds/{id}/directory` | Cached channels and roles |
| GET `/v1/guilds/{id}/stats` | Daily server statistics; optional `days` (1–30, default 7) |
| GET `/v1/guilds/{id}/access/{user_id}` | Member level and `dashboard.edit` decision |
| GET `/v1/guilds/{id}/config` | Stored configuration; optional `view` |
| PUT `/v1/guilds/{id}/config` | Save a configuration; optional `view` |
| POST `/v1/guilds/{id}/config/validate` | Validate without saving |
| GET `/v1/guilds/{id}/config/effective` | Configuration with defaults |
| GET `/v1/guilds/{id}/config/history` | Revision history; optional `limit` |

Configuration responses carry `ETag` with the revision. Supply it as `If-Match` for a PUT;
`"0"` creates the first revision. JSON is the default; use `Accept: application/yaml` for YAML.
The optional `X-Closure-Actor` contains the decimal Discord user ID and is recorded in history.
When this header is present, changes to `permissions` or `moderator_roles` require level 100.
An actor below level 100 also cannot introduce a new reference anywhere else in the document to a
role assigned a higher level in the saved `permissions`, or allowed by a saved permission key.
Numeric strings and numbers are treated as potential role IDs; existing references remain allowed.
Discord Administrator permissions on roles are not known from the bot configuration and are not
covered by this check.
Without it, a trusted operator using the admin token directly may change them.

Access returns `level`, `level_name`, `allowed` and `reason`; a non-member returns level 0 and is
denied. A BFF must also check access before every privileged operation, supply the authenticated
user's ID as `X-Closure-Actor`, and never trust a browser-provided actor ID.
Access checks and actor-attributed saves return 503 when the gateway is not ready or the guild is
unavailable.

Errors are JSON objects with an `error` containing a `code` and `message`. Common status codes:
400 malformed request, 401 authentication required, 403 forbidden, 404 server not joined,
412 stale revision, 422 invalid configuration, 428 missing `If-Match`, and 503 unavailable.
Rate limiting may return 429; oversized requests return 413.

## Server statistics

`GET /v1/guilds/{id}/stats?days=7` returns daily buckets from oldest to newest,
including today in the configured guild timezone (UTC by default), with `Cache-Control: no-store`.
`members.total` is Discord's current count or null when unavailable. Joins and leaves count
non-bot accounts only from the time recording began; only counts are stored, never member IDs.
The first day may be incomplete. Member activity older than 400 days is removed. A member day
before `members.since_unix` is null. `messages.since_unix` is the oldest retained
message; earlier days are null. Message counts depend on modules requesting message history and
are limited by `MESSAGE_HISTORY_DAYS` (`retention_days`, default 7); no message body is returned.
Cases cover all retained history; `recent` contains up to ten cases regardless of the window.
Times are Unix seconds, dates are `YYYY-MM-DD`, and Discord IDs are strings. A guild without
configuration returns empty counts and a null member total.
Member activity is stored in UTC hour buckets, so zones with a non-whole-hour offset cannot
split the bucket crossing midnight exactly; that bucket is assigned by its starting hour.
`members.since_unix` is the earliest retained bucket and moves forward after retention purges.

For example, a one-day empty result has this shape (timestamps and dates are illustrative):

```json
{"generated_at_unix":1793790000,"timezone":"UTC","days":1,
 "members":{"total":null,"since_unix":null,"joins":0,"leaves":0,"by_day":[{"date":"2026-11-04","joins":null,"leaves":null}]},
 "messages":{"retention_days":7,"since_unix":null,"total":0,"by_day":[{"date":"2026-11-04","count":null}]},
 "cases":{"total":0,"by_kind":{},"by_day":[{"date":"2026-11-04","kinds":{}}],"recent":[]}}
```
