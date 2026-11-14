-- Serves an author's keyset pages; the query orders by whole-second updated_at, then id.
CREATE INDEX ticket_gallery_owner_updated_order
    ON ticket_gallery_templates (author_id, updated_at DESC, id DESC);
