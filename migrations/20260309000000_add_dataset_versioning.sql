-- P3-W1: Dataset registry and versioning.
-- Extends dataset_versions with status and uniqueness constraints,
-- adds FK linkage from ledger_entries to dataset_versions.

-- ---------------------------------------------------------------------------
-- 1. Add status column to dataset_versions (default 'active' for backcompat)
-- ---------------------------------------------------------------------------

ALTER TABLE dataset_versions
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'active';

-- ---------------------------------------------------------------------------
-- 2. Add UNIQUE constraint on (dataset_name, version)
--    Prevents duplicate version numbers within a dataset.
-- ---------------------------------------------------------------------------

ALTER TABLE dataset_versions
    ADD CONSTRAINT uq_dataset_versions_name_version UNIQUE (dataset_name, version);

-- ---------------------------------------------------------------------------
-- 3. Add dataset_version_id FK on ledger_entries (nullable during transition)
--    Existing rows will have NULL; new materializations will populate this.
-- ---------------------------------------------------------------------------

ALTER TABLE ledger_entries
    ADD COLUMN IF NOT EXISTS dataset_version_id UUID REFERENCES dataset_versions(id);

-- ---------------------------------------------------------------------------
-- 4. Index on ledger_entries(dataset_version_id) for version-scoped queries
-- ---------------------------------------------------------------------------

CREATE INDEX IF NOT EXISTS idx_ledger_entries_dataset_version_id
    ON ledger_entries(dataset_version_id);
