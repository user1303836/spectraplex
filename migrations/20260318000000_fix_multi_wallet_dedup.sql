-- Fix #183: Shared transactions dropped or mis-linked across tracked wallets.
--
-- V1 transactions table has UNIQUE(chain, tx_hash), which silently discards
-- the second wallet's copy of a shared transaction. Change the constraint to
-- UNIQUE(chain, tx_hash, wallet_address) so the same on-chain transaction can
-- appear once per tracked wallet.
--
-- V2 raw_transactions already deduplicates correctly on (network, tx_hash),
-- but the RETURNING clause needs the ingested_at column to always resolve
-- the canonical row id. Add an updated_at column to raw_transactions so
-- the ON CONFLICT ... DO UPDATE SET updated_at = NOW() RETURNING id pattern
-- works reliably.

-- ---------------------------------------------------------------------------
-- 1. V1: Change unique constraint from (chain, tx_hash) to
--    (chain, tx_hash, wallet_address)
-- ---------------------------------------------------------------------------

ALTER TABLE transactions
    DROP CONSTRAINT IF EXISTS uq_transactions_chain_tx_hash;

ALTER TABLE transactions
    ADD CONSTRAINT uq_transactions_chain_tx_hash_wallet
    UNIQUE (chain, tx_hash, wallet_address);

-- ---------------------------------------------------------------------------
-- 2. V2: Add updated_at column to raw_transactions for upsert RETURNING
-- ---------------------------------------------------------------------------

ALTER TABLE raw_transactions
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
