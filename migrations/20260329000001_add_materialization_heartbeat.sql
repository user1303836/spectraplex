ALTER TABLE materialization_runs ADD COLUMN IF NOT EXISTS heartbeat_at TIMESTAMPTZ;
