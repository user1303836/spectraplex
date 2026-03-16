-- Add proper ON DELETE behavior to foreign key constraints.
-- Existing constraints default to NO ACTION, which causes orphaned records
-- when parent rows are deleted.
--
-- Strategy:
--   CASCADE  for strong parent-child (child has no meaning without parent)
--   SET NULL for optional references (child can survive without parent)

-- ---------------------------------------------------------------------------
-- 1. transactions -> ledger_entries  (CASCADE)
-- ---------------------------------------------------------------------------

ALTER TABLE ledger_entries
    DROP CONSTRAINT IF EXISTS ledger_entries_transaction_id_fkey,
    ADD CONSTRAINT ledger_entries_transaction_id_fkey
        FOREIGN KEY (transaction_id) REFERENCES transactions(id) ON DELETE CASCADE;

-- ---------------------------------------------------------------------------
-- 2. transactions -> evm_logs  (CASCADE)
-- ---------------------------------------------------------------------------

ALTER TABLE evm_logs
    DROP CONSTRAINT IF EXISTS evm_logs_tx_id_fkey,
    ADD CONSTRAINT evm_logs_tx_id_fkey
        FOREIGN KEY (tx_id) REFERENCES transactions(id) ON DELETE CASCADE;

-- ---------------------------------------------------------------------------
-- 3. transactions -> hl_fills  (CASCADE)
-- ---------------------------------------------------------------------------

ALTER TABLE hl_fills
    DROP CONSTRAINT IF EXISTS hl_fills_tx_id_fkey,
    ADD CONSTRAINT hl_fills_tx_id_fkey
        FOREIGN KEY (tx_id) REFERENCES transactions(id) ON DELETE CASCADE;

-- ---------------------------------------------------------------------------
-- 4. index_targets -> dataset_completeness  (CASCADE)
-- ---------------------------------------------------------------------------

ALTER TABLE dataset_completeness
    DROP CONSTRAINT IF EXISTS dataset_completeness_target_id_fkey,
    ADD CONSTRAINT dataset_completeness_target_id_fkey
        FOREIGN KEY (target_id) REFERENCES index_targets(id) ON DELETE CASCADE;

-- ---------------------------------------------------------------------------
-- 5. dataset_version_id references  (SET NULL)
--    These are optional FK columns; if the version is deleted the child
--    record remains valid with a NULL version pointer.
-- ---------------------------------------------------------------------------

ALTER TABLE ledger_entries
    DROP CONSTRAINT IF EXISTS ledger_entries_dataset_version_id_fkey,
    ADD CONSTRAINT ledger_entries_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE token_transfers
    DROP CONSTRAINT IF EXISTS token_transfers_dataset_version_id_fkey,
    ADD CONSTRAINT token_transfers_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE native_balance_deltas
    DROP CONSTRAINT IF EXISTS native_balance_deltas_dataset_version_id_fkey,
    ADD CONSTRAINT native_balance_deltas_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE decoded_events
    DROP CONSTRAINT IF EXISTS decoded_events_dataset_version_id_fkey,
    ADD CONSTRAINT decoded_events_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE hl_fill_records
    DROP CONSTRAINT IF EXISTS hl_fill_records_dataset_version_id_fkey,
    ADD CONSTRAINT hl_fill_records_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE hl_funding_payments
    DROP CONSTRAINT IF EXISTS hl_funding_payments_dataset_version_id_fkey,
    ADD CONSTRAINT hl_funding_payments_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE hl_position_changes
    DROP CONSTRAINT IF EXISTS hl_position_changes_dataset_version_id_fkey,
    ADD CONSTRAINT hl_position_changes_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE wallet_ledger
    DROP CONSTRAINT IF EXISTS wallet_ledger_dataset_version_id_fkey,
    ADD CONSTRAINT wallet_ledger_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE balance_history
    DROP CONSTRAINT IF EXISTS balance_history_dataset_version_id_fkey,
    ADD CONSTRAINT balance_history_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE hl_pnl_summary
    DROP CONSTRAINT IF EXISTS hl_pnl_summary_dataset_version_id_fkey,
    ADD CONSTRAINT hl_pnl_summary_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE hl_trade_history
    DROP CONSTRAINT IF EXISTS hl_trade_history_dataset_version_id_fkey,
    ADD CONSTRAINT hl_trade_history_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE protocol_events
    DROP CONSTRAINT IF EXISTS protocol_events_dataset_version_id_fkey,
    ADD CONSTRAINT protocol_events_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE pool_snapshots
    DROP CONSTRAINT IF EXISTS pool_snapshots_dataset_version_id_fkey,
    ADD CONSTRAINT pool_snapshots_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

ALTER TABLE dataset_completeness
    DROP CONSTRAINT IF EXISTS dataset_completeness_dataset_version_id_fkey,
    ADD CONSTRAINT dataset_completeness_dataset_version_id_fkey
        FOREIGN KEY (dataset_version_id) REFERENCES dataset_versions(id) ON DELETE SET NULL;

-- ---------------------------------------------------------------------------
-- 6. ingestion_run_id optional references  (SET NULL)
-- ---------------------------------------------------------------------------

ALTER TABLE raw_transactions
    DROP CONSTRAINT IF EXISTS fk_raw_transactions_ingestion_run,
    ADD CONSTRAINT fk_raw_transactions_ingestion_run
        FOREIGN KEY (ingestion_run_id) REFERENCES ingestion_runs(id) ON DELETE SET NULL;

ALTER TABLE dataset_completeness
    DROP CONSTRAINT IF EXISTS dataset_completeness_last_ingestion_run_id_fkey,
    ADD CONSTRAINT dataset_completeness_last_ingestion_run_id_fkey
        FOREIGN KEY (last_ingestion_run_id) REFERENCES ingestion_runs(id) ON DELETE SET NULL;
