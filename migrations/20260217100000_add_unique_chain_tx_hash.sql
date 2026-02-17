-- Add unique constraint on (chain, tx_hash) to prevent duplicate transaction ingestion.
-- The same tx_hash can exist on different chains, but must be unique within a chain.
-- Drop the old non-unique index first since the unique constraint implicitly creates one.
DROP INDEX IF EXISTS idx_transactions_tx_hash;
ALTER TABLE transactions ADD CONSTRAINT uq_transactions_chain_tx_hash UNIQUE (chain, tx_hash);
