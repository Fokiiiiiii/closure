-- Where a case came from (a command, a button, automod, done by hand in Discord, the dashboard, a
-- script, an accepted appeal). Cases made before this column are commands.
ALTER TABLE cases
    ADD COLUMN source TEXT NOT NULL DEFAULT 'command'
    CHECK (source IN ('command', 'button', 'automod', 'manual', 'dashboard', 'script', 'appeal'));
