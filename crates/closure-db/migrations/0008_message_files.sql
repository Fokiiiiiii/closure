-- Files kept for the attachments of messages (see closure_core::history::KeptFile). A file is kept
-- only when a module asks for its attachments, or when it is fetched at a deletion or an edit;
-- `data` is NULL when the file could not be fetched. Rows go with their messages' age, and the
-- oldest stored files of a guild go first when it is over its limit.
CREATE TABLE message_files (
    guild_id       BIGINT      NOT NULL CHECK (guild_id > 0),
    message_id     BIGINT      NOT NULL CHECK (message_id > 0),
    attachment_id  BIGINT      NOT NULL CHECK (attachment_id > 0),
    channel_id     BIGINT      NOT NULL CHECK (channel_id > 0),
    filename       TEXT        NOT NULL,
    size           BIGINT      NOT NULL CHECK (size >= 0),
    content_type   TEXT,
    url            TEXT        NOT NULL,
    stored         BOOLEAN     NOT NULL,
    data           BYTEA,
    kept_at        TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (guild_id, message_id, attachment_id),
    CHECK (stored = (data IS NOT NULL))
);

CREATE INDEX message_files_kept ON message_files (guild_id, kept_at);
CREATE INDEX message_files_age ON message_files (kept_at);
