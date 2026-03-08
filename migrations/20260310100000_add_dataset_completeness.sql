-- P3-W6: Completeness metadata.
-- Tracks dataset completeness per target and time range so downstream
-- consumers can determine whether a dataset is partial, complete, or
-- backfilling for a given target and network.

CREATE TABLE IF NOT EXISTS dataset_completeness (
    id                      UUID PRIMARY KEY,
    target_id               UUID NOT NULL REFERENCES index_targets(id),
    dataset_name            TEXT NOT NULL,
    dataset_version_id      UUID REFERENCES dataset_versions(id),
    network                 TEXT NOT NULL REFERENCES networks(id),
    status                  TEXT NOT NULL DEFAULT 'partial'
                            CHECK (status IN ('partial', 'complete', 'backfilling', 'gap')),
    coverage_start          BIGINT,
    coverage_end            BIGINT,
    block_start             BIGINT,
    block_end               BIGINT,
    last_ingestion_run_id   UUID REFERENCES ingestion_runs(id),
    records_count           BIGINT NOT NULL DEFAULT 0,
    gap_ranges              JSONB,
    notes                   TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT uq_dataset_completeness_target_dataset_network
        UNIQUE (target_id, dataset_name, network)
);

CREATE INDEX IF NOT EXISTS idx_dataset_completeness_target_id
    ON dataset_completeness(target_id);

CREATE INDEX IF NOT EXISTS idx_dataset_completeness_dataset_name
    ON dataset_completeness(dataset_name);

CREATE INDEX IF NOT EXISTS idx_dataset_completeness_status
    ON dataset_completeness(status);

CREATE INDEX IF NOT EXISTS idx_dataset_completeness_network
    ON dataset_completeness(network);
