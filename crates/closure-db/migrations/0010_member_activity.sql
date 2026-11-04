-- Counts only; member IDs are never stored.
CREATE TABLE guild_member_activity (
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    hour TIMESTAMPTZ NOT NULL,
    joins INTEGER NOT NULL DEFAULT 0 CHECK (joins >= 0),
    leaves INTEGER NOT NULL DEFAULT 0 CHECK (leaves >= 0),
    PRIMARY KEY (guild_id, hour)
);
