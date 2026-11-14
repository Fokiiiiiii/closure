-- Bounded retention purges find old rows by age.
CREATE INDEX ticket_notifications_created ON ticket_notifications (created_at);
CREATE INDEX ticket_notification_outbox_finished
    ON ticket_notification_outbox (completed_at) WHERE completed_at IS NOT NULL;
CREATE INDEX ticket_notification_outbox_abandoned
    ON ticket_notification_outbox (abandoned_at) WHERE abandoned_at IS NOT NULL;
