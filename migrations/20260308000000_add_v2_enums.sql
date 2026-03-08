-- P1-W2: V2 enum types for the target-centric indexing model.
-- Additive only — existing V1 enums (chain_enum, entry_type_enum) are untouched.

-- Chain family groups protocols that share ingestion primitives.
-- Values match ChainFamily Rust enum serde(rename_all = "lowercase").
CREATE TYPE chain_family_enum AS ENUM ('solana', 'evm', 'hyperliquid');

-- Eight target kinds per P0-W2 Section 2.
-- Values match TargetKind Rust enum serde(rename_all = "snake_case").
CREATE TYPE target_kind_enum AS ENUM (
    'wallet',
    'contract',
    'program',
    'account',
    'topic_filter',
    'market',
    'pool',
    'protocol'
);

-- Ingestion mode controlling which connector methods are invoked.
-- Values match TargetMode Rust enum serde(rename_all = "snake_case").
CREATE TYPE target_mode_enum AS ENUM ('backfill', 'stream', 'both');
