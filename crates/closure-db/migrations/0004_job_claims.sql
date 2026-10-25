-- Who holds a job, and when it was planned apart from when it is handed out next.
--
-- `claim_token` changes with every hand-out and when a job is replaced; what a runner reports
-- about a job (done, failed, postponed) only counts while the job is still held under its token.
-- `planned_at` is what a repeating job counts its period from: a retry or a postponement moves
-- `run_at` but not the period.

ALTER TABLE scheduled_jobs
    ADD COLUMN claim_token BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN planned_at  TIMESTAMPTZ;
UPDATE scheduled_jobs SET planned_at = run_at;
ALTER TABLE scheduled_jobs ALTER COLUMN planned_at SET NOT NULL;

CREATE SEQUENCE scheduled_job_claims;
