CREATE TABLE indexer_checkpoints (
    chain chain_enum NOT NULL,
    wallet_address TEXT NOT NULL,
    last_signature TEXT,
    last_slot BIGINT,
    last_timestamp BIGINT,
    updated_at TIMESTAMPTZ DEFAULT NOW(),
    PRIMARY KEY (chain, wallet_address)
);
