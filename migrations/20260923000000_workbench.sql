-- Incremental is the API's default job mode; it must be accepted by run lineage too.
ALTER TABLE ingestion_runs DROP CONSTRAINT chk_ingestion_runs_mode;
ALTER TABLE ingestion_runs ADD CONSTRAINT chk_ingestion_runs_mode
    CHECK (mode IN ('backfill', 'incremental', 'stream', 'both'));

-- Preserve every run's input set when multiple targets ingest the same transaction.
CREATE TABLE ingestion_run_transactions (
    ingestion_run_id UUID NOT NULL REFERENCES ingestion_runs(id) ON DELETE CASCADE,
    raw_transaction_id UUID NOT NULL REFERENCES raw_transactions(id) ON DELETE CASCADE,
    PRIMARY KEY (ingestion_run_id, raw_transaction_id)
);
CREATE INDEX idx_run_transactions_raw ON ingestion_run_transactions(raw_transaction_id);
INSERT INTO ingestion_run_transactions
SELECT ingestion_run_id, id FROM raw_transactions WHERE ingestion_run_id IS NOT NULL;

-- Synthetic examples never share a namespace with real chain data.
INSERT INTO networks (id, chain_family, display_name, is_testnet, finality_model) VALUES
    ('solana-demo', 'solana', 'Solana · sample data', TRUE, 'instant'),
    ('ethereum-demo', 'evm', 'Ethereum · sample data', TRUE, 'instant'),
    ('hypercore-demo', 'hyperliquid', 'Hyperliquid · sample data', TRUE, 'instant');
