-- Fix P1: Dead worker recovery for export_jobs.
--
-- Adds heartbeat_at column to export_jobs so stale running/delivering export
-- jobs can be detected and reclaimed, matching the pattern already used by
-- stream_subscriptions and ingestion_job_attempts.

ALTER TABLE export_jobs ADD COLUMN IF NOT EXISTS heartbeat_at TIMESTAMPTZ;

-- Index for finding stale in-progress export jobs by heartbeat age.
-- Covers both 'running' and 'delivering' phases since workers must heartbeat
-- throughout the full export lifecycle.
DROP INDEX IF EXISTS idx_export_jobs_heartbeat;
CREATE INDEX idx_export_jobs_heartbeat
    ON export_jobs (heartbeat_at)
    WHERE status IN ('running', 'delivering');
