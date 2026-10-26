-- Messages kept for modules that hold `message_history` (see closure_core::history). Rows are
-- removed by age, not by module, so they stay whether or not a module still needs them.

CREATE TABLE saved_messages (
    guild_id          BIGINT      NOT NULL CHECK (guild_id > 0),
    message_id        BIGINT      NOT NULL CHECK (message_id > 0),
    channel_id        BIGINT      NOT NULL CHECK (channel_id > 0),
    author_id         BIGINT      NOT NULL CHECK (author_id > 0),
    author_bot        BOOLEAN     NOT NULL,
    content           TEXT        NOT NULL,
    attachments       JSONB       NOT NULL DEFAULT '[]',
    embed_count       INTEGER     NOT NULL CHECK (embed_count >= 0),
    sticker_count     INTEGER     NOT NULL CHECK (sticker_count >= 0),
    mentioned_users   BIGINT[]    NOT NULL DEFAULT '{}',
    mentioned_roles   BIGINT[]    NOT NULL DEFAULT '{}',
    mentions_everyone BOOLEAN     NOT NULL,
    reply_to_id       BIGINT      CHECK (reply_to_id IS NULL OR reply_to_id > 0),
    created_at        TIMESTAMPTZ NOT NULL,
    edited_at         TIMESTAMPTZ,
    deleted_at        TIMESTAMPTZ,
    PRIMARY KEY (guild_id, message_id)
);

CREATE INDEX saved_messages_author ON saved_messages (guild_id, author_id, created_at DESC);
CREATE INDEX saved_messages_channel ON saved_messages (guild_id, channel_id, created_at DESC);
CREATE INDEX saved_messages_created ON saved_messages (created_at);
