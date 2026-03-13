-- Add CHECK constraints on TEXT columns that store enum-like values.
-- This prevents invalid data from reaching the database and documents
-- the expected domain of each column.

-- ingestion_runs.status: lifecycle states
ALTER TABLE ingestion_runs
    ADD CONSTRAINT chk_ingestion_runs_status
    CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled'));

-- ingestion_runs.mode: ingestion strategy
ALTER TABLE ingestion_runs
    ADD CONSTRAINT chk_ingestion_runs_mode
    CHECK (mode IN ('backfill', 'stream', 'both'));

-- dataset_versions.status: version lifecycle
ALTER TABLE dataset_versions
    ADD CONSTRAINT chk_dataset_versions_status
    CHECK (status IN ('active', 'superseded', 'failed'));

-- wallet_ledger.entry_type: ledger entry classification
ALTER TABLE wallet_ledger
    ADD CONSTRAINT chk_wallet_ledger_entry_type
    CHECK (entry_type IN ('trade', 'fee', 'transfer', 'staking', 'income', 'funding'));

-- networks.finality_model: chain finality semantics (kebab-case per FinalityModel enum)
ALTER TABLE networks
    ADD CONSTRAINT chk_networks_finality_model
    CHECK (finality_model IN ('probabilistic-slot', 'probabilistic-block', 'deterministic-block', 'instant'));

-- hl_fills.side: trade side
ALTER TABLE hl_fills
    ADD CONSTRAINT chk_hl_fills_side
    CHECK (side IN ('B', 'A'));

-- hl_fill_records.side: trade side (V2 silver table)
ALTER TABLE hl_fill_records
    ADD CONSTRAINT chk_hl_fill_records_side
    CHECK (side IN ('B', 'A'));
