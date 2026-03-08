-- P1-W2: Network registry table and seed data for 13 canonical networks.
-- Per P0-W3 Section 3.1 and 3.3.

CREATE TABLE networks (
    id              TEXT PRIMARY KEY,
    chain_family    chain_family_enum NOT NULL,
    display_name    TEXT NOT NULL,
    is_testnet      BOOLEAN NOT NULL DEFAULT FALSE,
    finality_model  TEXT NOT NULL,
    block_time_ms   INTEGER,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Seed data: 13 networks frozen for Phase 1 (P0-W3 Section 3.3).

-- Solana family (Section 3.3.1)
INSERT INTO networks (id, chain_family, display_name, is_testnet, finality_model, block_time_ms) VALUES
    ('solana-mainnet',    'solana',       'Solana Mainnet',     FALSE, 'probabilistic-slot',  400),
    ('solana-devnet',     'solana',       'Solana Devnet',      TRUE,  'probabilistic-slot',  400),
    ('solana-testnet',    'solana',       'Solana Testnet',     TRUE,  'probabilistic-slot',  400);

-- EVM family (Section 3.3.2)
INSERT INTO networks (id, chain_family, display_name, is_testnet, finality_model, block_time_ms) VALUES
    ('ethereum-mainnet',  'evm',          'Ethereum Mainnet',   FALSE, 'probabilistic-block', 12000),
    ('ethereum-sepolia',  'evm',          'Ethereum Sepolia',   TRUE,  'probabilistic-block', 12000),
    ('base-mainnet',      'evm',          'Base Mainnet',       FALSE, 'deterministic-block', 2000),
    ('base-sepolia',      'evm',          'Base Sepolia',       TRUE,  'deterministic-block', 2000),
    ('arbitrum-mainnet',  'evm',          'Arbitrum One',       FALSE, 'deterministic-block', 250),
    ('arbitrum-sepolia',  'evm',          'Arbitrum Sepolia',   TRUE,  'deterministic-block', 250),
    ('hyperevm-mainnet',  'evm',          'HyperEVM Mainnet',   FALSE, 'deterministic-block', 2000),
    ('hyperevm-testnet',  'evm',          'HyperEVM Testnet',   TRUE,  'deterministic-block', 2000);

-- Hyperliquid family (Section 3.3.3)
-- block_time_ms is NULL for HyperCore (blockless, instant finality).
INSERT INTO networks (id, chain_family, display_name, is_testnet, finality_model, block_time_ms) VALUES
    ('hypercore-mainnet', 'hyperliquid',  'HyperCore Mainnet',  FALSE, 'instant',             NULL),
    ('hypercore-testnet', 'hyperliquid',  'HyperCore Testnet',  TRUE,  'instant',             NULL);
