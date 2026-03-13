-- Add index on ledger_entries.transaction_id to improve JOIN performance
-- between the bronze (transactions) and silver (ledger_entries) layers.
CREATE INDEX IF NOT EXISTS idx_ledger_entries_transaction_id
    ON ledger_entries(transaction_id);
