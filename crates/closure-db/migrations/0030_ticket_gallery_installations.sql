-- One row per guild that imported a template. import_count is only raised when a new row
-- lands here; rows created before this table keep their stored import_count unchanged.
CREATE TABLE ticket_gallery_installations (
    guild_id BIGINT NOT NULL CHECK (guild_id > 0),
    template_id BIGINT NOT NULL REFERENCES ticket_gallery_templates (id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, template_id)
);
