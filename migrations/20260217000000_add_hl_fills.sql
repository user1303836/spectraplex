-- Hyperliquid fills extension table for detailed fill data
CREATE TABLE hl_fills (
    id BIGSERIAL PRIMARY KEY,
    tx_id UUID REFERENCES transactions(id),
    coin TEXT NOT NULL,
    side TEXT NOT NULL,
    price NUMERIC NOT NULL,
    size NUMERIC NOT NULL,
    direction TEXT,
    closed_pnl NUMERIC,
    fee NUMERIC,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX idx_hl_fills_tx_id ON hl_fills(tx_id);
CREATE INDEX idx_hl_fills_coin ON hl_fills(coin);
