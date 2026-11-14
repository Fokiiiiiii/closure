-- Records when the verification code was handed to SMTP, so a replayed request can tell
-- a delivered code from one that never left the process.
ALTER TABLE ticket_notification_preferences
    ADD COLUMN email_code_sent_at TIMESTAMPTZ;
