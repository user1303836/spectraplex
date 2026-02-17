-- Blocks table for reorg detection
CREATE TABLE blocks (
    id BIGSERIAL PRIMARY KEY,
    chain chain_enum NOT NULL,
    block_num BIGINT NOT NULL,
    block_hash TEXT,
    parent_hash TEXT,
    timestamp TIMESTAMPTZ NOT NULL,
    UNIQUE(chain, block_num)
);

CREATE INDEX idx_blocks_chain_num ON blocks(chain, block_num DESC);

-- EVM logs table for raw event storage
CREATE TABLE evm_logs (
    id BIGSERIAL PRIMARY KEY,
    tx_id UUID REFERENCES transactions(id),
    log_index INT NOT NULL,
    address TEXT NOT NULL,
    topics TEXT[],
    data BYTEA,
    created_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE INDEX idx_evm_logs_tx_id ON evm_logs(tx_id);
CREATE INDEX idx_evm_logs_address ON evm_logs(address);
