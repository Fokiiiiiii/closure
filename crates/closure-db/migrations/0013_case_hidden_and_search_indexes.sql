-- A case can be hidden from the lists, and the search of cases by moderator, kind and source needs
-- its indexes: a guild has up to a million cases.
ALTER TABLE cases ADD COLUMN hidden BOOLEAN NOT NULL DEFAULT false;
CREATE INDEX cases_moderator ON cases (guild_id, moderator, number DESC);
CREATE INDEX cases_kind ON cases (guild_id, kind, number DESC);
