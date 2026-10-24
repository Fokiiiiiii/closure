-- Guilds the bot has been in. Rows are kept after the bot leaves so history stays attributable.
CREATE TABLE guilds (
    id        BIGINT PRIMARY KEY CHECK (id > 0),
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    left_at   TIMESTAMPTZ
);

-- The current sparse configuration document of a guild. `revision` implements optimistic
-- concurrency: an edit only applies if the editor saw the latest revision.
CREATE TABLE guild_config (
    guild_id   BIGINT PRIMARY KEY REFERENCES guilds (id) ON DELETE CASCADE,
    revision   BIGINT NOT NULL CHECK (revision >= 1),
    doc        JSONB NOT NULL CHECK (jsonb_typeof(doc) = 'object'),
    updated_by BIGINT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Every revision ever saved, for audit and rollback.
CREATE TABLE guild_config_history (
    guild_id   BIGINT NOT NULL REFERENCES guilds (id) ON DELETE CASCADE,
    revision   BIGINT NOT NULL,
    doc        JSONB NOT NULL,
    actor      BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, revision)
);

-- Actions the bot took, independent of Discord's own audit log. No foreign key on purpose:
-- the trail must outlive any guild row.
CREATE TABLE audit_events (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    guild_id   BIGINT NOT NULL,
    actor      BIGINT,
    action     TEXT NOT NULL CHECK (char_length(action) BETWEEN 1 AND 64),
    payload    JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_guild_created ON audit_events (guild_id, created_at DESC, id DESC);
