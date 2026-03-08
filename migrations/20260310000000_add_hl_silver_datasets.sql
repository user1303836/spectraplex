-- P3-W4: Hyperliquid reusable Silver datasets.
-- Normalized fill records, funding payments, and position changes.
--
-- NOTE: The existing V1 extension table `hl_fills` (from 20260217000000) is
-- left untouched. These Silver tables use UUID primary keys and reference
-- the V2 `raw_transactions` table.

-- ---------------------------------------------------------------------------
-- 1. hl_fill_records — normalized fill (trade execution) records
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS hl_fill_records (
    id                  UUID PRIMARY KEY,
    raw_transaction_id  UUID REFERENCES raw_transactions(id),
    network             TEXT NOT NULL,
    coin                TEXT NOT NULL,
    side                TEXT NOT NULL,
    price               NUMERIC NOT NULL,
    size                NUMERIC NOT NULL,
    direction           TEXT,
    closed_pnl          NUMERIC,
    fee                 NUMERIC,
    fee_token           TEXT,
    fill_time           BIGINT NOT NULL,
    order_id            BIGINT,
    trade_id            BIGINT,
    dataset_version_id  UUID REFERENCES dataset_versions(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_hl_fill_records_coin
    ON hl_fill_records(coin);

CREATE INDEX IF NOT EXISTS idx_hl_fill_records_raw_tx
    ON hl_fill_records(raw_transaction_id);

CREATE INDEX IF NOT EXISTS idx_hl_fill_records_network
    ON hl_fill_records(network);

CREATE INDEX IF NOT EXISTS idx_hl_fill_records_dataset_version
    ON hl_fill_records(dataset_version_id);

CREATE INDEX IF NOT EXISTS idx_hl_fill_records_fill_time
    ON hl_fill_records(fill_time);

CREATE UNIQUE INDEX IF NOT EXISTS uq_hl_fill_records_dedup
    ON hl_fill_records(raw_transaction_id, coin, fill_time, side)
    WHERE raw_transaction_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- 2. hl_funding_payments — normalized funding payment records
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS hl_funding_payments (
    id                  UUID PRIMARY KEY,
    raw_transaction_id  UUID REFERENCES raw_transactions(id),
    network             TEXT NOT NULL,
    coin                TEXT NOT NULL,
    amount              NUMERIC NOT NULL,
    funding_rate        NUMERIC,
    payment_time        BIGINT NOT NULL,
    dataset_version_id  UUID REFERENCES dataset_versions(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_hl_funding_payments_coin
    ON hl_funding_payments(coin);

CREATE INDEX IF NOT EXISTS idx_hl_funding_payments_raw_tx
    ON hl_funding_payments(raw_transaction_id);

CREATE INDEX IF NOT EXISTS idx_hl_funding_payments_network
    ON hl_funding_payments(network);

CREATE INDEX IF NOT EXISTS idx_hl_funding_payments_dataset_version
    ON hl_funding_payments(dataset_version_id);

CREATE INDEX IF NOT EXISTS idx_hl_funding_payments_payment_time
    ON hl_funding_payments(payment_time);

CREATE UNIQUE INDEX IF NOT EXISTS uq_hl_funding_payments_dedup
    ON hl_funding_payments(raw_transaction_id, coin, payment_time)
    WHERE raw_transaction_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- 3. hl_position_changes — normalized position state change records
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS hl_position_changes (
    id                  UUID PRIMARY KEY,
    raw_transaction_id  UUID REFERENCES raw_transactions(id),
    network             TEXT NOT NULL,
    coin                TEXT NOT NULL,
    side                TEXT NOT NULL,
    size_delta          NUMERIC NOT NULL,
    price               NUMERIC NOT NULL,
    direction           TEXT,
    source_event        TEXT NOT NULL,
    dataset_version_id  UUID REFERENCES dataset_versions(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_hl_position_changes_coin
    ON hl_position_changes(coin);

CREATE INDEX IF NOT EXISTS idx_hl_position_changes_raw_tx
    ON hl_position_changes(raw_transaction_id);

CREATE INDEX IF NOT EXISTS idx_hl_position_changes_network
    ON hl_position_changes(network);

CREATE INDEX IF NOT EXISTS idx_hl_position_changes_dataset_version
    ON hl_position_changes(dataset_version_id);

CREATE UNIQUE INDEX IF NOT EXISTS uq_hl_position_changes_dedup
    ON hl_position_changes(raw_transaction_id, coin, side, source_event)
    WHERE raw_transaction_id IS NOT NULL;
