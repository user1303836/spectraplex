-- P3-W2: Token transfers Silver dataset.
-- Normalized token transfer records across all chains.

-- ---------------------------------------------------------------------------
-- 1. Create token_transfers table
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS token_transfers (
    id                  UUID PRIMARY KEY,
    raw_transaction_id  UUID REFERENCES raw_transactions(id),
    network             TEXT NOT NULL,
    token_address       TEXT NOT NULL,
    token_symbol        TEXT,
    from_address        TEXT NOT NULL,
    to_address          TEXT NOT NULL,
    amount              NUMERIC NOT NULL,
    decimals            INT NOT NULL,
    dataset_version_id  UUID REFERENCES dataset_versions(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ---------------------------------------------------------------------------
-- 2. Indexes for common query patterns
-- ---------------------------------------------------------------------------

-- Lookup by either address involved in a transfer (generic, not wallet-specific).
CREATE INDEX IF NOT EXISTS idx_token_transfers_from_address
    ON token_transfers(from_address);

CREATE INDEX IF NOT EXISTS idx_token_transfers_to_address
    ON token_transfers(to_address);

-- Lookup by raw transaction for provenance tracking.
CREATE INDEX IF NOT EXISTS idx_token_transfers_raw_tx
    ON token_transfers(raw_transaction_id);

-- Lookup by network for chain-scoped queries.
CREATE INDEX IF NOT EXISTS idx_token_transfers_network
    ON token_transfers(network);

-- Lookup by dataset version for version-scoped queries.
CREATE INDEX IF NOT EXISTS idx_token_transfers_dataset_version
    ON token_transfers(dataset_version_id);

-- Uniqueness: one transfer record per raw transaction + from + to + token.
-- This prevents duplicate materialization of the same transfer.
CREATE UNIQUE INDEX IF NOT EXISTS uq_token_transfers_dedup
    ON token_transfers(raw_transaction_id, from_address, to_address, token_address, amount)
    WHERE raw_transaction_id IS NOT NULL;
