CREATE TABLE ticket_notification_preferences (
    user_id BIGINT PRIMARY KEY CHECK (user_id > 0),
    in_app_enabled BOOLEAN NOT NULL DEFAULT TRUE,
    discord_dm_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    email_enabled BOOLEAN NOT NULL DEFAULT FALSE,
    revision BIGINT NOT NULL DEFAULT 0 CHECK (revision >= 0),
    locale TEXT NOT NULL DEFAULT 'en' CHECK (locale IN ('en', 'ja')),
    email_ciphertext BYTEA,
    email_masked TEXT CHECK (email_masked IS NULL OR char_length(email_masked) <= 254),
    pending_email_ciphertext BYTEA,
    pending_email_masked TEXT CHECK (pending_email_masked IS NULL OR char_length(pending_email_masked) <= 254),
    email_verified BOOLEAN NOT NULL DEFAULT FALSE,
    email_code_hash BYTEA CHECK (email_code_hash IS NULL OR octet_length(email_code_hash) = 32),
    email_code_expires_at TIMESTAMPTZ,
    email_code_attempts SMALLINT NOT NULL DEFAULT 0 CHECK (email_code_attempts BETWEEN 0 AND 5),
    email_requested_at TIMESTAMPTZ,
    last_email_request_key TEXT CHECK (last_email_request_key IS NULL OR last_email_request_key ~ '^[A-Za-z0-9_-]{1,64}$'),
    last_email_request_fingerprint BYTEA CHECK (
        last_email_request_fingerprint IS NULL OR octet_length(last_email_request_fingerprint) = 32
    ),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (NOT email_verified OR email_ciphertext IS NOT NULL),
    CHECK (NOT email_enabled OR email_verified),
    CHECK ((email_code_hash IS NULL) = (email_code_expires_at IS NULL)),
    CHECK (email_code_hash IS NULL OR pending_email_ciphertext IS NOT NULL)
);

CREATE TABLE ticket_notifications (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id BIGINT NOT NULL CHECK (user_id > 0),
    source_event_id BIGINT NOT NULL,
    source_guild_id BIGINT NOT NULL CHECK (source_guild_id > 0),
    category TEXT NOT NULL CHECK (category = 'gallery'),
    event_type TEXT NOT NULL CHECK (event_type IN ('gallery.approved', 'gallery.rejected')),
    submission_title TEXT NOT NULL CHECK (char_length(submission_title) BETWEEN 1 AND 100),
    reason TEXT CHECK (reason IS NULL OR char_length(reason) <= 500),
    href TEXT NOT NULL CHECK (
        href LIKE '/g/%/tickets/gallery/submissions%'
        AND position('://' IN href) = 0
    ),
    created_at TIMESTAMPTZ NOT NULL,
    read_at TIMESTAMPTZ,
    UNIQUE (user_id, source_event_id)
);

CREATE INDEX ticket_notifications_user_page
    ON ticket_notifications (user_id, id DESC);
CREATE INDEX ticket_notifications_user_unread
    ON ticket_notifications (user_id, id DESC) WHERE read_at IS NULL;

CREATE TABLE ticket_notification_outbox (
    event_id BIGINT PRIMARY KEY
        REFERENCES ticket_gallery_events (event_id) ON DELETE CASCADE,
    available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    in_app_done BOOLEAN NOT NULL DEFAULT FALSE,
    discord_dm_done BOOLEAN NOT NULL DEFAULT FALSE,
    email_done BOOLEAN NOT NULL DEFAULT FALSE,
    completed_at TIMESTAMPTZ,
    abandoned_at TIMESTAMPTZ,
    last_failure TEXT CHECK (
        last_failure IS NULL OR last_failure IN ('transient', 'recipient_unavailable', 'delivery_exhausted', 'permanent')
    ),
    CHECK (completed_at IS NULL OR abandoned_at IS NULL)
);

CREATE INDEX ticket_notification_outbox_due
    ON ticket_notification_outbox (available_at, event_id)
    WHERE completed_at IS NULL AND abandoned_at IS NULL;
