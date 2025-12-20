-- Add wallet_address to ledger_entries for direct lookup
ALTER TABLE ledger_entries ADD COLUMN wallet_address VARCHAR(255);

-- Populate existing rows (Backfill)
-- We join with transactions to find the correct wallet for each ledger entry
UPDATE ledger_entries le
SET wallet_address = tx.wallet_address
FROM transactions tx
WHERE le.transaction_id = tx.id;

-- Enforce Not Null after backfill
ALTER TABLE ledger_entries ALTER COLUMN wallet_address SET NOT NULL;

-- Index for fast tax report generation
CREATE INDEX idx_ledger_wallet_created ON ledger_entries(wallet_address, created_at);
