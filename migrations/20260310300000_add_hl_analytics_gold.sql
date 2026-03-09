-- P5-W2: Gold-tier Hyperliquid analytics tables
-- hl_pnl_summary: per-wallet per-coin PnL summaries aggregated from Silver fills and funding
-- hl_trade_history: logical trade records grouped from Silver fills

CREATE TABLE IF NOT EXISTS hl_pnl_summary (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    wallet_address  TEXT NOT NULL,
    coin            TEXT NOT NULL,
    network         TEXT NOT NULL,
    period_start    BIGINT NOT NULL,
    period_end      BIGINT NOT NULL,
    total_closed_pnl NUMERIC NOT NULL DEFAULT 0,
    total_funding   NUMERIC NOT NULL DEFAULT 0,
    total_fees      NUMERIC NOT NULL DEFAULT 0,
    net_pnl         NUMERIC NOT NULL DEFAULT 0,
    trade_count     BIGINT NOT NULL DEFAULT 0,
    fill_count      BIGINT NOT NULL DEFAULT 0,
    avg_trade_size  NUMERIC NOT NULL DEFAULT 0,
    win_count       BIGINT NOT NULL DEFAULT 0,
    loss_count      BIGINT NOT NULL DEFAULT 0,
    dataset_version_id UUID REFERENCES dataset_versions(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_hl_pnl_summary_wallet_coin
    ON hl_pnl_summary (wallet_address, coin);
CREATE INDEX IF NOT EXISTS idx_hl_pnl_summary_network
    ON hl_pnl_summary (network);
CREATE INDEX IF NOT EXISTS idx_hl_pnl_summary_period_start
    ON hl_pnl_summary (period_start);
CREATE INDEX IF NOT EXISTS idx_hl_pnl_summary_period_end
    ON hl_pnl_summary (period_end);

CREATE TABLE IF NOT EXISTS hl_trade_history (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    wallet_address  TEXT NOT NULL,
    coin            TEXT NOT NULL,
    network         TEXT NOT NULL,
    side            TEXT NOT NULL,
    entry_price     NUMERIC NOT NULL,
    exit_price      NUMERIC NOT NULL,
    size            NUMERIC NOT NULL,
    opened_at       BIGINT NOT NULL,
    closed_at       BIGINT NOT NULL,
    realized_pnl    NUMERIC NOT NULL DEFAULT 0,
    fees            NUMERIC NOT NULL DEFAULT 0,
    num_fills       BIGINT NOT NULL DEFAULT 0,
    dataset_version_id UUID REFERENCES dataset_versions(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_hl_trade_history_wallet_coin
    ON hl_trade_history (wallet_address, coin);
CREATE INDEX IF NOT EXISTS idx_hl_trade_history_network
    ON hl_trade_history (network);
CREATE INDEX IF NOT EXISTS idx_hl_trade_history_opened_at
    ON hl_trade_history (opened_at DESC);
CREATE INDEX IF NOT EXISTS idx_hl_trade_history_closed_at
    ON hl_trade_history (closed_at DESC);
