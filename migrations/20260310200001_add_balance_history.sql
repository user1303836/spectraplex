-- Balance history table: per-wallet, per-asset running balance snapshots
-- derived from Silver data for point-in-time forensics and portfolio tracking.
CREATE TABLE IF NOT EXISTS balance_history (
    id              UUID PRIMARY KEY,
    wallet_address  TEXT NOT NULL,
    asset_symbol    TEXT NOT NULL,
    network         TEXT NOT NULL,
    timestamp       BIGINT NOT NULL,
    balance         NUMERIC NOT NULL,
    tx_hash         TEXT NOT NULL,
    dataset_version_id UUID REFERENCES dataset_versions(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_balance_history_wallet_asset_ts
    ON balance_history (wallet_address, asset_symbol, timestamp DESC);

CREATE INDEX IF NOT EXISTS idx_balance_history_network
    ON balance_history (network);
