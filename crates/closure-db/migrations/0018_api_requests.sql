-- Requests the API was asked to carry out once (see `Db::claim_request`): the dashboard sends a
-- key with a message, and a second request with the same key does not send it again.
CREATE TABLE api_requests (
    guild_id   bigint      NOT NULL,
    actor      text        NOT NULL,
    key        text        NOT NULL,
    request    jsonb       NOT NULL,
    response   jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, actor, key)
);

CREATE INDEX api_requests_created ON api_requests (created_at);
