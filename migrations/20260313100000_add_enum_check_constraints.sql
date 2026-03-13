-- Add CHECK constraints on TEXT columns that store enum-like values.
-- This prevents invalid data from reaching the database and documents
-- the expected domain of each column.
--
-- Each constraint is wrapped in a DO block so the migration is idempotent:
-- if the constraint already exists, the duplicate_object exception is caught
-- and silently ignored.

-- ingestion_runs.status: lifecycle states
DO $$ BEGIN
    ALTER TABLE ingestion_runs
        ADD CONSTRAINT chk_ingestion_runs_status
        CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled'));
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- ingestion_runs.mode: ingestion strategy
DO $$ BEGIN
    ALTER TABLE ingestion_runs
        ADD CONSTRAINT chk_ingestion_runs_mode
        CHECK (mode IN ('backfill', 'stream', 'both'));
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- dataset_versions.status: version lifecycle
DO $$ BEGIN
    ALTER TABLE dataset_versions
        ADD CONSTRAINT chk_dataset_versions_status
        CHECK (status IN ('active', 'superseded', 'failed'));
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- wallet_ledger.entry_type: ledger entry classification
DO $$ BEGIN
    ALTER TABLE wallet_ledger
        ADD CONSTRAINT chk_wallet_ledger_entry_type
        CHECK (entry_type IN ('trade', 'fee', 'transfer', 'staking', 'income', 'funding'));
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- networks.finality_model: chain finality semantics (kebab-case per FinalityModel enum)
DO $$ BEGIN
    ALTER TABLE networks
        ADD CONSTRAINT chk_networks_finality_model
        CHECK (finality_model IN ('probabilistic-slot', 'probabilistic-block', 'deterministic-block', 'instant'));
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- hl_fills.side: trade side
DO $$ BEGIN
    ALTER TABLE hl_fills
        ADD CONSTRAINT chk_hl_fills_side
        CHECK (side IN ('B', 'A'));
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- hl_fill_records.side: trade side (V2 silver table)
DO $$ BEGIN
    ALTER TABLE hl_fill_records
        ADD CONSTRAINT chk_hl_fill_records_side
        CHECK (side IN ('B', 'A'));
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

-- hl_position_changes.side: trade side (position state changes)
DO $$ BEGIN
    ALTER TABLE hl_position_changes
        ADD CONSTRAINT chk_hl_position_changes_side
        CHECK (side IN ('B', 'A'));
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;
