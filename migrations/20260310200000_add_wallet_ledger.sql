-- Gold-tier wallet_ledger table: materialized wallet-scoped records with
-- counterparty tracking, network awareness, and nullable cost basis / proceeds
-- for downstream tax and forensics consumers.
CREATE TABLE IF NOT EXISTS wallet_ledger (
    id              UUID PRIMARY KEY,
    raw_transaction_id UUID REFERENCES raw_transactions(id),
    wallet_address  TEXT NOT NULL,
    network         TEXT NOT NULL,
    tx_hash         TEXT NOT NULL,
    timestamp       BIGINT NOT NULL,
    entry_type      TEXT NOT NULL,
    asset_symbol    TEXT NOT NULL,
    amount          NUMERIC NOT NULL,
    counterparty_address TEXT,
    fee_amount      NUMERIC,
    fee_asset       TEXT,
    cost_basis      NUMERIC,
    proceeds        NUMERIC,
    dataset_version_id UUID REFERENCES dataset_versions(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_wallet_ledger_wallet_ts
    ON wallet_ledger (wallet_address, timestamp DESC);

CREATE INDEX IF NOT EXISTS idx_wallet_ledger_network
    ON wallet_ledger (network);

CREATE INDEX IF NOT EXISTS idx_wallet_ledger_raw_tx
    ON wallet_ledger (raw_transaction_id);
