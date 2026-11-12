CREATE TABLE integration_secret_generations (
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    module_id TEXT NOT NULL CHECK (length(module_id) BETWEEN 1 AND 128),
    integration_id TEXT NOT NULL CHECK (integration_id ~ '^[a-z0-9]{1,32}$'),
    generation TEXT NOT NULL CHECK (generation ~ '^[0-9a-f]{32}$'),
    retired BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at TIMESTAMPTZ,
    PRIMARY KEY (guild_id, module_id, integration_id, generation)
);

CREATE TABLE integration_secrets (
    guild_id BIGINT NOT NULL,
    module_id TEXT NOT NULL,
    integration_id TEXT NOT NULL,
    generation TEXT NOT NULL,
    secret_name TEXT NOT NULL CHECK (secret_name ~ '^[a-z0-9_-]{1,64}$'),
    encrypted_value BYTEA NOT NULL
        CHECK (octet_length(encrypted_value) BETWEEN 29 AND 16413),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, module_id, integration_id, generation, secret_name),
    FOREIGN KEY (guild_id, module_id, integration_id, generation)
        REFERENCES integration_secret_generations
            (guild_id, module_id, integration_id, generation)
        ON DELETE CASCADE
);


CREATE TABLE integration_secret_receipts (
    guild_id BIGINT NOT NULL,
    module_id TEXT NOT NULL,
    integration_id TEXT NOT NULL,
    generation TEXT NOT NULL,
    secret_name TEXT NOT NULL,
    actor_id BIGINT NOT NULL CHECK (actor_id > 0),
    request_key_hmac BYTEA NOT NULL CHECK (octet_length(request_key_hmac) = 32),
    fingerprint_hmac BYTEA NOT NULL CHECK (octet_length(fingerprint_hmac) = 32),
    encrypted_receipt BYTEA NOT NULL
        CHECK (octet_length(encrypted_receipt) BETWEEN 29 AND 128),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (
        guild_id, module_id, integration_id, generation, secret_name,
        actor_id, request_key_hmac
    ),
    FOREIGN KEY (guild_id, module_id, integration_id, generation)
        REFERENCES integration_secret_generations
            (guild_id, module_id, integration_id, generation)
        ON DELETE CASCADE
);

CREATE INDEX integration_secret_receipts_expiry
    ON integration_secret_receipts (expires_at);
