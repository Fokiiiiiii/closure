-- The verification mail is an outbox row. The sealed payload lets a stopped request resume
-- with the same code and Message-ID instead of issuing a new code. email_code_sent_at is kept
-- and set together with the 'sent' state, so existing readers keep working.
ALTER TABLE ticket_notification_preferences
    ADD COLUMN email_send_state TEXT NOT NULL DEFAULT 'none'
        CHECK (email_send_state IN ('none', 'queued', 'sent', 'failed')),
    ADD COLUMN email_payload_ciphertext BYTEA;

UPDATE ticket_notification_preferences
SET email_send_state = CASE
    WHEN email_code_sent_at IS NOT NULL THEN 'sent'
    WHEN email_code_hash IS NOT NULL THEN 'queued'
    ELSE 'none'
END;
