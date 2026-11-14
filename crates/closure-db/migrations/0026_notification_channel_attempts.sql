-- Each channel keeps its own attempt count, next run time and final status, so a failing
-- channel is backed off or given up without holding back the others. Existing rows inherit
-- the shared attempt counter, which is what they were charged so far.
ALTER TABLE ticket_notification_outbox
    ADD COLUMN event_failures INTEGER NOT NULL DEFAULT 0 CHECK (event_failures >= 0),
    ADD COLUMN in_app_attempts INTEGER NOT NULL DEFAULT 0 CHECK (in_app_attempts >= 0),
    ADD COLUMN discord_dm_attempts INTEGER NOT NULL DEFAULT 0 CHECK (discord_dm_attempts >= 0),
    ADD COLUMN email_attempts INTEGER NOT NULL DEFAULT 0 CHECK (email_attempts >= 0),
    ADD COLUMN in_app_next_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN discord_dm_next_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN email_next_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN in_app_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (in_app_status IN ('pending', 'sent', 'failed', 'skipped')),
    ADD COLUMN discord_dm_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (discord_dm_status IN ('pending', 'sent', 'failed', 'skipped')),
    ADD COLUMN email_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (email_status IN ('pending', 'sent', 'unknown', 'failed', 'skipped'));

UPDATE ticket_notification_outbox
SET in_app_attempts = attempts,
    discord_dm_attempts = attempts,
    email_attempts = attempts;

-- Rows finished before this migration did not record the outcome; a finished channel is shown as sent.
UPDATE ticket_notification_outbox
SET in_app_status = CASE WHEN in_app_done THEN 'sent' ELSE 'pending' END,
    discord_dm_status = CASE WHEN discord_dm_done THEN 'sent' ELSE 'pending' END,
    email_status = CASE WHEN email_done THEN 'sent' ELSE 'pending' END;
