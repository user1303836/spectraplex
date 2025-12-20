-- Enums
CREATE TYPE chain_enum AS ENUM ('solana', 'hyperliquid', 'ethereum');
CREATE TYPE entry_type_enum AS ENUM ('trade', 'fee', 'transfer', 'staking', 'income');

-- Bronze Layer: Raw Transactions
CREATE TABLE transactions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL,
    wallet_address VARCHAR(255) NOT NULL,
    timestamp BIGINT NOT NULL,
    tx_hash VARCHAR(255) NOT NULL,
    chain chain_enum NOT NULL,
    raw_metadata JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for fast lookup by wallet and time
CREATE INDEX idx_transactions_wallet_time ON transactions(wallet_address, timestamp);
CREATE INDEX idx_transactions_tx_hash ON transactions(tx_hash);


-- Silver Layer: Normalized Ledger Entries
CREATE TABLE ledger_entries (
    id UUID PRIMARY KEY,
    transaction_id UUID REFERENCES transactions(id),
    user_id UUID NOT NULL,
    asset_symbol VARCHAR(50) NOT NULL,
    amount NUMERIC NOT NULL,
    entry_type entry_type_enum NOT NULL,
    fiat_value NUMERIC,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Index for tax calculations (User + Time)
CREATE INDEX idx_ledger_user_created ON ledger_entries(user_id, created_at);
