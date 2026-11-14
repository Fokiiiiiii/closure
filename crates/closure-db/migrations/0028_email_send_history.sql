-- One row per verification mail that was reserved, whether or not SMTP accepted it, so a
-- failed send still counts toward the quota.
CREATE TABLE ticket_notification_email_sends (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id BIGINT NOT NULL CHECK (account_id > 0),
    recipient_hash BYTEA NOT NULL CHECK (octet_length(recipient_hash) = 32),
    reserved_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ticket_notification_email_sends_recipient
    ON ticket_notification_email_sends (recipient_hash, reserved_at);
CREATE INDEX ticket_notification_email_sends_account
    ON ticket_notification_email_sends (account_id, reserved_at);
CREATE INDEX ticket_notification_email_sends_time
    ON ticket_notification_email_sends (reserved_at);
