-- A DM component's guild cannot be inferred from its channel or trusted from its custom_id.
-- The host records the message it sent and expires that provenance with the ticket history.
CREATE TABLE dm_component_routes (
    message_id BIGINT PRIMARY KEY CHECK (message_id > 0),
    value      JSONB       NOT NULL,
    revision   BIGINT      NOT NULL CHECK (revision >= 1),
    expires_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX dm_component_routes_expiry ON dm_component_routes (expires_at);
