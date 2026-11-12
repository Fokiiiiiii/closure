CREATE TABLE ticket_gallery_templates (
    id BIGSERIAL PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('panel', 'tag', 'form')),
    title TEXT NOT NULL CHECK (char_length(title) BETWEEN 1 AND 100),
    description TEXT NOT NULL CHECK (char_length(description) BETWEEN 1 AND 500),
    category TEXT NOT NULL CHECK (category IN (
        'support', 'moderation', 'sales', 'applications', 'feedback', 'general', 'other'
    )),
    tags JSONB NOT NULL DEFAULT '[]'::jsonb
        CHECK (jsonb_typeof(tags) = 'array' AND jsonb_array_length(tags) <= 3),
    payload JSONB NOT NULL
        CHECK (jsonb_typeof(payload) = 'object' AND octet_length(payload::text) <= 65536),
    author_id BIGINT NOT NULL CHECK (author_id > 0),
    source_guild_id BIGINT NOT NULL CHECK (source_guild_id > 0),
    author_name TEXT NOT NULL CHECK (char_length(author_name) BETWEEN 1 AND 80),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'rejected', 'withdrawn', 'unpublished')),
    review_reason TEXT CHECK (review_reason IS NULL OR char_length(review_reason) <= 500),
    featured BOOLEAN NOT NULL DEFAULT FALSE,
    import_count BIGINT NOT NULL DEFAULT 0 CHECK (import_count >= 0),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    published_at TIMESTAMPTZ
);

CREATE INDEX ticket_gallery_approved_order
    ON ticket_gallery_templates (status, featured DESC, published_at DESC, id DESC);
CREATE INDEX ticket_gallery_owner_order
    ON ticket_gallery_templates (author_id, created_at DESC, id DESC);

CREATE TABLE ticket_gallery_requests (
    actor_id BIGINT NOT NULL CHECK (actor_id > 0),
    request_key TEXT NOT NULL CHECK (request_key ~ '^[A-Za-z0-9_-]{1,64}$'),
    action TEXT NOT NULL CHECK (action IN ('submit', 'update', 'withdraw', 'unpublish', 'import', 'review_approved', 'review_rejected')),
    fingerprint BYTEA NOT NULL CHECK (octet_length(fingerprint) = 32),
    template_id BIGINT NOT NULL REFERENCES ticket_gallery_templates (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (actor_id, request_key)
);

CREATE TABLE ticket_gallery_events (
    event_id BIGSERIAL PRIMARY KEY,
    submission_id BIGINT NOT NULL REFERENCES ticket_gallery_templates (id) ON DELETE CASCADE,
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    revision BIGINT NOT NULL CHECK (revision > 0),
    recipient_id BIGINT NOT NULL CHECK (recipient_id > 0),
    reviewer_id BIGINT NOT NULL CHECK (reviewer_id > 0),
    event_type TEXT NOT NULL CHECK (event_type IN ('gallery.approved', 'gallery.rejected')),
    status TEXT NOT NULL CHECK (status IN ('approved', 'rejected')),
    reason TEXT CHECK (reason IS NULL OR char_length(reason) <= 500),
    title TEXT NOT NULL CHECK (char_length(title) BETWEEN 1 AND 100),
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (submission_id, revision)
);

CREATE INDEX ticket_gallery_events_recipient
    ON ticket_gallery_events (recipient_id, event_id);
