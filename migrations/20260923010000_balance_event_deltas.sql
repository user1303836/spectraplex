-- Persist event deltas so older arrivals can recompute the existing suffix safely.
-- Legacy rows remain NULL: never invent a delta for an untraceable old snapshot.
ALTER TABLE balance_history ADD COLUMN delta NUMERIC;
