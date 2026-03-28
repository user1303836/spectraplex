-- Add provenance and delivery destination columns to export_jobs.
--
-- delivery_destination: where the export was actually delivered (webhook URL,
-- local-file path). Separate from result_location (on-disk artifact path)
-- so downloads and status can both be correct.
--
-- Provenance fields capture dataset version and completeness state at the
-- time the export was produced, preventing drift when versions change later.

ALTER TABLE export_jobs ADD COLUMN IF NOT EXISTS delivery_destination TEXT;
ALTER TABLE export_jobs ADD COLUMN IF NOT EXISTS dataset_version_id UUID;
ALTER TABLE export_jobs ADD COLUMN IF NOT EXISTS dataset_version INT;
ALTER TABLE export_jobs ADD COLUMN IF NOT EXISTS completeness_status TEXT;
ALTER TABLE export_jobs ADD COLUMN IF NOT EXISTS completeness_coverage JSONB;
ALTER TABLE export_jobs ADD COLUMN IF NOT EXISTS last_ingestion_run_id UUID;
