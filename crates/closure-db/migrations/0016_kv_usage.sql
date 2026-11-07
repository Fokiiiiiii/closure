-- How many entries each module keeps in each guild, kept by the table itself, so a new entry
-- learns whether the quota has room from one row and not by counting the entries.
CREATE TABLE module_kv_usage (
    module   TEXT   NOT NULL CHECK (char_length(module) BETWEEN 1 AND 32),
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    entries  BIGINT NOT NULL CHECK (entries >= 0),
    PRIMARY KEY (module, guild_id)
);

INSERT INTO module_kv_usage (module, guild_id, entries)
SELECT module, guild_id, count(*) FROM module_kv GROUP BY module, guild_id;

CREATE FUNCTION module_kv_usage_apply() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO module_kv_usage (module, guild_id, entries) VALUES (NEW.module, NEW.guild_id, 1)
        ON CONFLICT (module, guild_id) DO UPDATE SET entries = module_kv_usage.entries + 1;
    ELSE
        UPDATE module_kv_usage SET entries = GREATEST(entries - 1, 0)
        WHERE module = OLD.module AND guild_id = OLD.guild_id;
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER module_kv_usage_insert AFTER INSERT ON module_kv
FOR EACH ROW EXECUTE FUNCTION module_kv_usage_apply();
CREATE TRIGGER module_kv_usage_delete AFTER DELETE ON module_kv
FOR EACH ROW EXECUTE FUNCTION module_kv_usage_apply();
