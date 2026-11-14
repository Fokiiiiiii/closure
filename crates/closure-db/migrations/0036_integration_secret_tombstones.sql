-- A dropped secret name leaves this row behind: the fact of the removal, without any value.
-- While it exists the name reads as having no value, until a new value is stored.
CREATE TABLE integration_secret_tombstones (
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    module_id TEXT NOT NULL CHECK (length(module_id) BETWEEN 1 AND 128),
    integration_id TEXT NOT NULL CHECK (integration_id ~ '^[a-z0-9]{1,32}$'),
    generation TEXT NOT NULL CHECK (generation ~ '^[0-9a-f]{32}$'),
    secret_name TEXT NOT NULL CHECK (secret_name ~ '^[a-z0-9_-]{1,64}$'),
    removed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, module_id, integration_id, generation, secret_name),
    FOREIGN KEY (guild_id, module_id, integration_id, generation)
        REFERENCES integration_secret_generations
            (guild_id, module_id, integration_id, generation)
        ON DELETE CASCADE
);
