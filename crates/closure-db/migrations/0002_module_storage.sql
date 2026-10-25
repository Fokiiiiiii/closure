-- What modules keep: entries and counters, per module and guild. There is no foreign key to
-- `guilds`: the data stays with the module whether or not the bot is in the guild right now.

CREATE TABLE module_kv (
    module     TEXT        NOT NULL CHECK (char_length(module) BETWEEN 1 AND 32),
    guild_id   BIGINT      NOT NULL CHECK (guild_id > 0),
    key        TEXT        NOT NULL CHECK (char_length(key) BETWEEN 1 AND 128),
    value      JSONB       NOT NULL,
    -- 1 for a new entry, one more for every write (compare-and-set).
    revision   BIGINT      NOT NULL CHECK (revision >= 1),
    expires_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (module, guild_id, key)
);

CREATE INDEX module_kv_expiry ON module_kv (expires_at) WHERE expires_at IS NOT NULL;

-- A counter has a name and a subject (a user, a role, ...). Its total is the row with bucket 0;
-- the other rows hold what was added in each minute (bucket = minutes since the epoch), so
-- "the last hour" can be asked.
CREATE TABLE module_counters (
    module   TEXT   NOT NULL CHECK (char_length(module) BETWEEN 1 AND 32),
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    name     TEXT   NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    subject  BIGINT NOT NULL,
    bucket   BIGINT NOT NULL CHECK (bucket >= 0),
    value    BIGINT NOT NULL,
    PRIMARY KEY (module, guild_id, name, subject, bucket)
);

CREATE INDEX module_counters_minutes ON module_counters (bucket) WHERE bucket > 0;
