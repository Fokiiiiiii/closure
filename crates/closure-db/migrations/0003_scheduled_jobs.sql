-- Work that modules asked to have done later, once or again and again. Like the module data,
-- jobs stay with the module and the guild whether or not the bot is in the guild right now.

CREATE TABLE scheduled_jobs (
    id            BIGINT      GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    module        TEXT        NOT NULL CHECK (char_length(module) BETWEEN 1 AND 32),
    guild_id      BIGINT      NOT NULL CHECK (guild_id > 0),
    name          TEXT        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    payload       JSONB       NOT NULL,
    run_at        TIMESTAMPTZ NOT NULL,
    -- Set for a job that repeats: the time between two planned runs.
    every_seconds BIGINT      CHECK (every_seconds IS NULL OR every_seconds >= 10),
    -- A job with the same key (of the same module and guild) is replaced, not added.
    dedupe_key    TEXT        CHECK (char_length(dedupe_key) BETWEEN 1 AND 128),
    -- How many times it was taken to run in this round (repeating jobs start again at 0).
    attempts      INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- While a runner has the job: until when. After that it is handed out again.
    leased_until  TIMESTAMPTZ,
    -- `failed` jobs are kept for a look and never handed out.
    state         TEXT        NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'failed')),
    last_error    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX scheduled_jobs_key ON scheduled_jobs (module, guild_id, dedupe_key)
    WHERE dedupe_key IS NOT NULL;
CREATE INDEX scheduled_jobs_due ON scheduled_jobs (run_at) WHERE state = 'pending';
CREATE INDEX scheduled_jobs_owner ON scheduled_jobs (module, guild_id);
