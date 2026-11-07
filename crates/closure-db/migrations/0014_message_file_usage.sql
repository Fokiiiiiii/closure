-- The stored bytes of each guild's kept files, kept by the table itself, so a save does not add
-- the whole guild's files up again to learn whether it is over its limit.
CREATE TABLE message_file_usage (
    guild_id BIGINT NOT NULL PRIMARY KEY CHECK (guild_id > 0),
    bytes    BIGINT NOT NULL CHECK (bytes >= 0)
);

INSERT INTO message_file_usage (guild_id, bytes)
SELECT guild_id, SUM(octet_length(data)) FROM message_files WHERE stored GROUP BY guild_id;

CREATE FUNCTION message_file_usage_apply() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    gid   BIGINT;
    delta BIGINT := 0;
BEGIN
    IF TG_OP <> 'INSERT' THEN
        gid := OLD.guild_id;
        delta := delta - COALESCE(octet_length(OLD.data), 0);
    END IF;
    IF TG_OP <> 'DELETE' THEN
        gid := NEW.guild_id;
        delta := delta + COALESCE(octet_length(NEW.data), 0);
    END IF;
    IF delta <> 0 THEN
        INSERT INTO message_file_usage (guild_id, bytes) VALUES (gid, GREATEST(delta, 0))
        ON CONFLICT (guild_id)
        DO UPDATE SET bytes = GREATEST(message_file_usage.bytes + delta, 0);
    END IF;
    RETURN NULL;
END
$$;

CREATE TRIGGER message_files_usage
AFTER INSERT OR UPDATE OR DELETE ON message_files
FOR EACH ROW EXECUTE FUNCTION message_file_usage_apply();
