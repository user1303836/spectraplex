-- P3-W2: Native balance deltas Silver dataset.
-- Native currency balance changes per account per transaction.

-- ---------------------------------------------------------------------------
-- 1. Create native_balance_deltas table
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS native_balance_deltas (
    id                  UUID PRIMARY KEY,
    raw_transaction_id  UUID REFERENCES raw_transactions(id),
    network             TEXT NOT NULL,
    account_address     TEXT NOT NULL,
    native_token        TEXT NOT NULL,
    pre_balance         NUMERIC NOT NULL,
    post_balance        NUMERIC NOT NULL,
    delta               NUMERIC NOT NULL,
    is_fee_payer        BOOLEAN NOT NULL DEFAULT FALSE,
    dataset_version_id  UUID REFERENCES dataset_versions(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- ---------------------------------------------------------------------------
-- 2. Indexes for common query patterns
-- ---------------------------------------------------------------------------

-- Lookup by account address (generic, not wallet-specific).
CREATE INDEX IF NOT EXISTS idx_native_balance_deltas_account
    ON native_balance_deltas(account_address);

-- Lookup by raw transaction for provenance tracking.
CREATE INDEX IF NOT EXISTS idx_native_balance_deltas_raw_tx
    ON native_balance_deltas(raw_transaction_id);

-- Lookup by network for chain-scoped queries.
CREATE INDEX IF NOT EXISTS idx_native_balance_deltas_network
    ON native_balance_deltas(network);

-- Lookup by dataset version for version-scoped queries.
CREATE INDEX IF NOT EXISTS idx_native_balance_deltas_dataset_version
    ON native_balance_deltas(dataset_version_id);

-- Uniqueness: one delta record per raw transaction + account.
-- This prevents duplicate materialization of the same balance change.
CREATE UNIQUE INDEX IF NOT EXISTS uq_native_balance_deltas_dedup
    ON native_balance_deltas(raw_transaction_id, account_address)
    WHERE raw_transaction_id IS NOT NULL;
