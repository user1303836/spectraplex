-- P3-W3: Decoded events Silver dataset.
-- Normalized decoded log events (EVM) and program instructions (Solana).

-- ---------------------------------------------------------------------------
-- 1. Create decoded_events table
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS decoded_events (
    id                    UUID PRIMARY KEY,
    raw_transaction_id    UUID REFERENCES raw_transactions(id),
    network               TEXT NOT NULL,
    chain_family          chain_family_enum NOT NULL,
    program_or_contract   TEXT NOT NULL,
    event_signature       TEXT,
    event_name            TEXT,
    log_index             INT NOT NULL,
    decoded_fields        JSONB NOT NULL DEFAULT '{}',
    raw_fields            JSONB NOT NULL DEFAULT '{}',
    dataset_version_id    UUID REFERENCES dataset_versions(id),
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ---------------------------------------------------------------------------
-- 2. Indexes for common query patterns
-- ---------------------------------------------------------------------------

-- Lookup by contract/program address.
CREATE INDEX IF NOT EXISTS idx_decoded_events_program_or_contract
    ON decoded_events(program_or_contract);

-- Lookup by event signature (EVM topic0 or instruction discriminator).
CREATE INDEX IF NOT EXISTS idx_decoded_events_event_signature
    ON decoded_events(event_signature);

-- Lookup by raw transaction for provenance tracking.
CREATE INDEX IF NOT EXISTS idx_decoded_events_raw_tx
    ON decoded_events(raw_transaction_id);

-- Lookup by network for chain-scoped queries.
CREATE INDEX IF NOT EXISTS idx_decoded_events_network
    ON decoded_events(network);

-- Lookup by dataset version for version-scoped queries.
CREATE INDEX IF NOT EXISTS idx_decoded_events_dataset_version
    ON decoded_events(dataset_version_id);

-- Uniqueness: one decoded event per raw transaction + contract + log index.
-- This prevents duplicate materialization of the same event.
CREATE UNIQUE INDEX IF NOT EXISTS uq_decoded_events_dedup
    ON decoded_events(raw_transaction_id, program_or_contract, log_index)
    WHERE raw_transaction_id IS NOT NULL;
