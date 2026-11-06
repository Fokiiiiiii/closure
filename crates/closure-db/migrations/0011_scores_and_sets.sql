-- What grows with the members of a guild: a score for each (experience, points) and a place in a
-- set for each (who entered a giveaway). Neither takes a lock nor counts a quota, so a write
-- costs the same whether a guild has a hundred members or a million, and a ranking, a place and a
-- random draw are answered by the index, not by reading everything.

CREATE TABLE module_scores (
    module   TEXT   NOT NULL CHECK (char_length(module) BETWEEN 1 AND 32),
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    name     TEXT   NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    subject  BIGINT NOT NULL,
    value    BIGINT NOT NULL,
    PRIMARY KEY (module, guild_id, name, subject)
);

CREATE INDEX module_scores_ranking ON module_scores (module, guild_id, name, value DESC, subject);

CREATE TABLE module_sets (
    module   TEXT   NOT NULL CHECK (char_length(module) BETWEEN 1 AND 32),
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    name     TEXT   NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    member   BIGINT NOT NULL,
    PRIMARY KEY (module, guild_id, name, member)
);
