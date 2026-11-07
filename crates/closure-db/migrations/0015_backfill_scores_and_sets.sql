-- What leveling, giveaway and starboard kept as entries of the store before 0011 gave them scores
-- and sets: moved there, so an upgrade does not leave the experience, the entries of a running
-- giveaway and the posts of the board behind. A row that is there already (made since) is added
-- to, not replaced. The entries are removed once copied: they would otherwise take the quota of
-- the store, and this runs once.

INSERT INTO module_scores (module, guild_id, name, subject, value)
SELECT module, guild_id, 'xp', substring(key FROM 4)::bigint, (value ->> 'xp')::bigint
FROM module_kv
WHERE module = 'leveling'
  AND key ~ '^xp:[0-9]{1,18}$'
  AND (value ->> 'xp') ~ '^[0-9]{1,18}$'
  AND (value ->> 'xp')::bigint > 0
ON CONFLICT (module, guild_id, name, subject)
DO UPDATE SET value = module_scores.value + EXCLUDED.value;

DELETE FROM module_kv WHERE module = 'leveling' AND key ~ '^xp:[0-9]{1,18}$';

INSERT INTO module_sets (module, guild_id, name, member)
SELECT module, guild_id, 'e-' || split_part(key, ':', 2), split_part(key, ':', 3)::bigint
FROM module_kv
WHERE module = 'giveaway'
  AND key ~ '^e:[0-9]{1,18}:[0-9]{1,18}$'
ON CONFLICT DO NOTHING;

DELETE FROM module_kv WHERE module = 'giveaway' AND key ~ '^e:[0-9]{1,18}:[0-9]{1,18}$';

INSERT INTO module_scores (module, guild_id, name, subject, value)
SELECT module, guild_id, 'post', substring(key FROM 6)::bigint, (value ->> 'post')::bigint
FROM module_kv
WHERE module = 'starboard'
  AND key ~ '^post:[0-9]{1,18}$'
  AND (value ->> 'post') ~ '^[0-9]{1,18}$'
  AND (value ->> 'post')::bigint > 0
ON CONFLICT (module, guild_id, name, subject) DO NOTHING;

DELETE FROM module_kv WHERE module = 'starboard' AND key ~ '^post:[0-9]{1,18}$';
