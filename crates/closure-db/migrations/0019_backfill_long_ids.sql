-- 0015 moved what leveling, giveaway and starboard kept as entries into scores and sets, but it
-- took only ids of 1 to 18 digits. An id of 19 digits is as valid (every account made since 2022
-- has one), and those entries were left in `module_kv`, where the plugins no longer read them.
--
-- This moves them. Each statement deletes only the rows it copies (the delete feeds the insert),
-- so a row that cannot be copied is not lost. A number is read only when it fits a BIGINT.

CREATE FUNCTION pg_temp.fitting_id(text) RETURNS bigint
LANGUAGE sql IMMUTABLE AS $$
    SELECT CASE
        WHEN $1 ~ '^[0-9]{1,19}$' AND (length($1) < 19 OR $1 <= '9223372036854775807')
        THEN $1::bigint
    END
$$;

WITH moved AS (
    DELETE FROM module_kv
    WHERE module = 'leveling'
      AND key ~ '^xp:[0-9]+$'
      AND pg_temp.fitting_id(substring(key FROM 4)) IS NOT NULL
      AND coalesce(pg_temp.fitting_id(value ->> 'xp'), 0) > 0
    RETURNING module, guild_id, key, value
)
INSERT INTO module_scores (module, guild_id, name, subject, value)
SELECT module, guild_id, 'xp', pg_temp.fitting_id(substring(key FROM 4)),
       pg_temp.fitting_id(value ->> 'xp')
FROM moved
ON CONFLICT (module, guild_id, name, subject)
DO UPDATE SET value = module_scores.value + EXCLUDED.value;

WITH moved AS (
    DELETE FROM module_kv
    WHERE module = 'giveaway'
      AND key ~ '^e:[0-9]+:[0-9]+$'
      AND pg_temp.fitting_id(split_part(key, ':', 2)) IS NOT NULL
      AND pg_temp.fitting_id(split_part(key, ':', 3)) IS NOT NULL
    RETURNING module, guild_id, key
)
INSERT INTO module_sets (module, guild_id, name, member)
SELECT module, guild_id, 'e-' || split_part(key, ':', 2),
       pg_temp.fitting_id(split_part(key, ':', 3))
FROM moved
ON CONFLICT DO NOTHING;

WITH moved AS (
    DELETE FROM module_kv
    WHERE module = 'starboard'
      AND key ~ '^post:[0-9]+$'
      AND pg_temp.fitting_id(substring(key FROM 6)) IS NOT NULL
      AND coalesce(pg_temp.fitting_id(value ->> 'post'), 0) > 0
    RETURNING module, guild_id, key, value
)
INSERT INTO module_scores (module, guild_id, name, subject, value)
SELECT module, guild_id, 'post', pg_temp.fitting_id(substring(key FROM 6)),
       pg_temp.fitting_id(value ->> 'post')
FROM moved
ON CONFLICT (module, guild_id, name, subject) DO NOTHING;
