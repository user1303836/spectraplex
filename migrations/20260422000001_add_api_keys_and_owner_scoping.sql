-- Workstream G / Issue #216: per-user/tenant isolation at the query layer.
--
-- Adds api_keys table for DB-backed key authentication with owner scoping,
-- and an index on index_targets(owner_id) to support owner-filtered queries.

-- ---------------------------------------------------------------------------
-- 1. api_keys: durable API key registry with owner scoping
-- ---------------------------------------------------------------------------

CREATE TABLE api_keys (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    key_hash     TEXT NOT NULL,
    name         TEXT,
    owner_id     UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at   TIMESTAMPTZ,

    CONSTRAINT uq_api_keys_key_hash UNIQUE (key_hash)
);

CREATE INDEX idx_api_keys_owner ON api_keys(owner_id);

-- ---------------------------------------------------------------------------
-- 2. Index for owner-scoped target lookups
-- ---------------------------------------------------------------------------

CREATE INDEX idx_index_targets_owner ON index_targets(owner_id)
    WHERE owner_id IS NOT NULL;
