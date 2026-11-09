-- Who changed a case, when, and what it was before: the edit of a case keeps its trail.
CREATE TABLE case_edits (
    id            BIGSERIAL   PRIMARY KEY,
    guild_id      BIGINT      NOT NULL CHECK (guild_id > 0),
    case_number   BIGINT      NOT NULL,
    edited_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    editor        BIGINT,
    old_reason    TEXT,
    new_reason    TEXT,
    old_hidden    BOOLEAN     NOT NULL,
    new_hidden    BOOLEAN     NOT NULL,
    old_moderator BIGINT      NOT NULL,
    new_moderator BIGINT      NOT NULL
);

CREATE INDEX case_edits_by_case ON case_edits (guild_id, case_number, id);
