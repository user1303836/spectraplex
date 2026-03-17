-- Bronze layer: raw EVM trace data from debug_traceTransaction / trace_transaction.
--
-- Stores the full JSON trace response per transaction so downstream Silver
-- materializers (e.g. NativeBalanceDelta) can extract balance changes,
-- internal transfers, and gas refund information.

-- ---------------------------------------------------------------------------
-- 1. Create raw_evm_traces table
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS raw_evm_traces (
    id                  UUID PRIMARY KEY,
    transaction_hash    TEXT NOT NULL,
    block_number        BIGINT,
    network             TEXT NOT NULL,
    trace_type          TEXT NOT NULL,
    raw_trace           JSONB NOT NULL,
    ingestion_run_id    UUID REFERENCES ingestion_runs(id) ON DELETE SET NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT raw_evm_traces_trace_type_check
        CHECK (trace_type IN ('callTracer', 'prestateTracer', 'flatCallTracer', 'other'))
);

-- ---------------------------------------------------------------------------
-- 2. Indexes
-- ---------------------------------------------------------------------------

-- Primary lookup: find trace by transaction hash, network, and trace type.
-- Includes trace_type so that multiple tracer outputs (e.g. callTracer and
-- prestateTracer) can coexist for the same transaction.
CREATE UNIQUE INDEX IF NOT EXISTS uq_raw_evm_traces_tx_network_type
    ON raw_evm_traces(network, transaction_hash, trace_type);

-- Lookup by block number for range-based queries and reorg handling.
CREATE INDEX IF NOT EXISTS idx_raw_evm_traces_block_number
    ON raw_evm_traces(network, block_number);

-- Lookup by ingestion run for provenance tracking.
CREATE INDEX IF NOT EXISTS idx_raw_evm_traces_ingestion_run
    ON raw_evm_traces(ingestion_run_id)
    WHERE ingestion_run_id IS NOT NULL;
