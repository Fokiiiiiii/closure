-- The counts of 0014 (kept files) and 0016 (entries) were made, and their triggers created, in
-- separate steps: a write between the two was left out of them. They are counted again here with
-- the writers held back for the moment, so that the numbers and the rows agree from now on.

LOCK TABLE message_files IN SHARE MODE;

INSERT INTO message_file_usage (guild_id, bytes)
SELECT guild_id, SUM(octet_length(data)) FROM message_files WHERE stored GROUP BY guild_id
ON CONFLICT (guild_id) DO UPDATE SET bytes = EXCLUDED.bytes;

UPDATE message_file_usage AS usage SET bytes = 0
WHERE bytes <> 0
  AND NOT EXISTS (
      SELECT 1 FROM message_files AS file WHERE file.guild_id = usage.guild_id AND file.stored);

LOCK TABLE module_kv IN SHARE MODE;

INSERT INTO module_kv_usage (module, guild_id, entries)
SELECT module, guild_id, count(*) FROM module_kv GROUP BY module, guild_id
ON CONFLICT (module, guild_id) DO UPDATE SET entries = EXCLUDED.entries;

UPDATE module_kv_usage AS usage SET entries = 0
WHERE entries <> 0
  AND NOT EXISTS (
      SELECT 1 FROM module_kv AS entry WHERE entry.module = usage.module AND entry.guild_id = usage.guild_id);
