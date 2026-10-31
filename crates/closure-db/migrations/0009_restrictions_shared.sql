-- The roles a restriction took off a member were kept in the storage of each module that stripped
-- them: `stripped:<user>`, and `stripped:<user>:timeout` or `:vcmute` before that. Every module now
-- reads one list per member, kept by the `restrictions` module, so the old lists are moved into it
-- once. A member's roles become the union of his lists; the list stays until the latest of its
-- expiries, or for good when one of them has none. Lists that already ran out are removed without
-- moving.

WITH sources AS (
    SELECT guild_id, split_part(key, ':', 2) AS user_id, value, expires_at
    FROM module_kv
    WHERE jsonb_typeof(value) = 'array'
      AND ((module <> 'restrictions' AND key ~ '^stripped:[0-9]+(:(timeout|vcmute))?$')
           OR (module = 'restrictions' AND key ~ '^stripped:[0-9]+$'))
),
merged AS (
    SELECT sources.guild_id,
           sources.user_id,
           jsonb_agg(DISTINCT roles.role ORDER BY roles.role) AS roles,
           CASE WHEN bool_or(sources.expires_at IS NULL) THEN NULL ELSE max(sources.expires_at) END
               AS expires_at
    FROM sources
    CROSS JOIN LATERAL jsonb_array_elements(sources.value) AS roles(role)
    WHERE sources.expires_at IS NULL OR sources.expires_at > now()
    GROUP BY sources.guild_id, sources.user_id
)
INSERT INTO module_kv (module, guild_id, key, value, revision, expires_at, updated_at)
SELECT 'restrictions', guild_id, 'stripped:' || user_id, roles, nextval('module_kv_revisions'),
       expires_at, now()
FROM merged
ON CONFLICT (module, guild_id, key) DO UPDATE
    SET value = EXCLUDED.value,
        revision = EXCLUDED.revision,
        expires_at = EXCLUDED.expires_at,
        updated_at = now();

DELETE FROM module_kv
WHERE module <> 'restrictions' AND key ~ '^stripped:[0-9]+(:(timeout|vcmute))?$';
