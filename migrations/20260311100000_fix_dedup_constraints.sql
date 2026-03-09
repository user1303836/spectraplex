-- Fix dedup constraints for token_transfers and native_balance_deltas.
--
-- token_transfers: Including `amount` in the unique index rejects legitimate
-- records when the same (tx, from, to, token) group has multiple transfers
-- with different amounts.  Replace with a (raw_transaction_id, from_address,
-- to_address, token_address, transfer_index) constraint and add the
-- transfer_index column.
--
-- native_balance_deltas: The existing constraint omits `native_token`, which
-- rejects legitimate records when the same account has deltas for different
-- native tokens in the same transaction.  Add `native_token` to the index.

-- ---------------------------------------------------------------------------
-- 1. token_transfers: add transfer_index column and fix unique index
-- ---------------------------------------------------------------------------

ALTER TABLE token_transfers
    ADD COLUMN IF NOT EXISTS transfer_index INT NOT NULL DEFAULT 0;

DROP INDEX IF EXISTS uq_token_transfers_dedup;

CREATE UNIQUE INDEX IF NOT EXISTS uq_token_transfers_dedup
    ON token_transfers(raw_transaction_id, from_address, to_address, token_address, transfer_index)
    WHERE raw_transaction_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- 2. native_balance_deltas: fix unique index to include native_token
-- ---------------------------------------------------------------------------

DROP INDEX IF EXISTS uq_native_balance_deltas_dedup;

CREATE UNIQUE INDEX IF NOT EXISTS uq_native_balance_deltas_dedup
    ON native_balance_deltas(raw_transaction_id, account_address, native_token)
    WHERE raw_transaction_id IS NOT NULL;
