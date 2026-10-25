-- Moderation cases, numbered per guild. The numbers come from a counter row of their own, so
-- every module that records cases shares one sequence per guild. Like the module data, cases
-- stay whether or not the bot is in the guild right now.

CREATE TABLE guild_case_numbers (
    guild_id BIGINT PRIMARY KEY CHECK (guild_id > 0),
    last     BIGINT NOT NULL CHECK (last >= 0)
);

CREATE TABLE cases (
    guild_id       BIGINT      NOT NULL CHECK (guild_id > 0),
    number         BIGINT      NOT NULL CHECK (number >= 1),
    module         TEXT        NOT NULL CHECK (char_length(module) BETWEEN 1 AND 32),
    kind           TEXT        NOT NULL CHECK (char_length(kind) BETWEEN 1 AND 32),
    target         BIGINT      NOT NULL CHECK (target > 0),
    moderator      BIGINT      NOT NULL CHECK (moderator > 0),
    reason         TEXT        CHECK (reason IS NULL OR char_length(reason) <= 512),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at     TIMESTAMPTZ,
    -- Set together when the case is revoked, once.
    revoked_by     BIGINT,
    revoked_at     TIMESTAMPTZ,
    revoked_reason TEXT        CHECK (revoked_reason IS NULL OR char_length(revoked_reason) <= 512),
    PRIMARY KEY (guild_id, number)
);

CREATE INDEX cases_target ON cases (guild_id, target, number DESC);
