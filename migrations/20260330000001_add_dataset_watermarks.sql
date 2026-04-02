-- Dataset watermarks track the last-processed position for each materialization
-- dataset + scope combination, enabling Bronze-range-driven materialization.
CREATE TABLE IF NOT EXISTS dataset_watermarks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    dataset_name TEXT NOT NULL,
    scope JSONB,
    last_ingestion_run_id UUID,
    last_raw_transaction_id UUID,
    last_processed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE NULLS NOT DISTINCT (dataset_name, scope)
);

CREATE INDEX IF NOT EXISTS idx_dataset_watermarks_dataset ON dataset_watermarks (dataset_name);
