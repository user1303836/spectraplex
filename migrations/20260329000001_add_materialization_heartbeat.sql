ALTER TABLE materialization_runs ADD COLUMN IF NOT EXISTS heartbeat_at TIMESTAMPTZ;

-- Backfill existing running rows so they are not immediately reclaimed as
-- stale during a rolling deploy (FIX 3/6: NULL heartbeat_at treated as stale).
UPDATE materialization_runs SET heartbeat_at = NOW() WHERE status = 'running' AND heartbeat_at IS NULL;
