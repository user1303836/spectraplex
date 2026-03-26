-- Fix P1: Dead worker recovery for export_jobs.
--
-- Adds heartbeat_at column to export_jobs so stale running export jobs
-- can be detected and reclaimed, matching the pattern already used by
-- stream_subscriptions and ingestion_job_attempts.

ALTER TABLE export_jobs ADD COLUMN IF NOT EXISTS heartbeat_at TIMESTAMPTZ;

-- Index for finding stale running export jobs by heartbeat age.
CREATE INDEX IF NOT EXISTS idx_export_jobs_heartbeat
    ON export_jobs (heartbeat_at)
    WHERE status = 'running';
