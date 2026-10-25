-- A revision is a number no two writes share, so that one taken before an entry was deleted (or
-- expired) and made again can never be mistaken for the new entry's.

CREATE SEQUENCE module_kv_revisions;
SELECT setval('module_kv_revisions', COALESCE((SELECT max(revision) FROM module_kv), 0) + 1, false);
