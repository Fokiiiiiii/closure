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
Without it, a trusted operator using the admin token directly may change them.

Access returns `level`, `level_name`, `allowed` and `reason`; a non-member returns level 0 and is
denied. A BFF must also check access before every privileged operation, supply the authenticated
user's ID as `X-Closure-Actor`, and never trust a browser-provided actor ID.

Errors are JSON objects with an `error` containing a `code` and `message`. Common status codes:
400 malformed request, 401 authentication required, 403 forbidden, 404 server not joined,
412 stale revision, 422 invalid configuration, 428 missing `If-Match`, and 503 unavailable.
Rate limiting may return 429; oversized requests return 413.
