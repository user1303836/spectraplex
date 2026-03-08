-- P1-W2: V2 control-plane and bronze tables.
-- Additive only — existing V1 tables are untouched.

-- ---------------------------------------------------------------------------
-- index_targets: registered indexing subjects (P0-W1 Section 3.2, P0-W2 Section 6)
-- ---------------------------------------------------------------------------

CREATE TABLE index_targets (
    id              UUID PRIMARY KEY,
    kind            target_kind_enum NOT NULL,
    network         TEXT NOT NULL REFERENCES networks(id),
    chain_family    chain_family_enum NOT NULL,
    address         TEXT,
    filter_spec     JSONB,
    filter_spec_hash TEXT,
    mode            target_mode_enum NOT NULL,
    label           TEXT,
    owner_id        UUID,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Uniqueness for simple (address-based) targets: one target per kind+network+address.
CREATE UNIQUE INDEX uq_index_targets_address
    ON index_targets (kind, network, address)
    WHERE address IS NOT NULL;

-- Uniqueness for complex (filter_spec-based) targets: one target per kind+network+filter_spec_hash.
CREATE UNIQUE INDEX uq_index_targets_filter_spec
    ON index_targets (kind, network, filter_spec_hash)
    WHERE filter_spec_hash IS NOT NULL;

CREATE INDEX idx_index_targets_network ON index_targets(network);
CREATE INDEX idx_index_targets_kind_network ON index_targets(kind, network);

-- ---------------------------------------------------------------------------
-- raw_transactions: canonical bronze layer (P0-W1 Section 3.1)
-- No wallet_address, no user_id — target-agnostic by design.
-- ---------------------------------------------------------------------------

CREATE TABLE raw_transactions (
    id               UUID PRIMARY KEY,
    network          TEXT NOT NULL REFERENCES networks(id),
    tx_hash          TEXT NOT NULL,
    timestamp        BIGINT NOT NULL,
    block_number     BIGINT,
    raw_metadata     JSONB NOT NULL,
    source           TEXT NOT NULL,
    ingestion_run_id UUID,
    ingested_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE raw_transactions
    ADD CONSTRAINT uq_raw_transactions_network_tx_hash UNIQUE (network, tx_hash);

CREATE INDEX idx_raw_transactions_network_hash ON raw_transactions(network, tx_hash);
CREATE INDEX idx_raw_transactions_timestamp ON raw_transactions(timestamp);
CREATE INDEX idx_raw_transactions_run ON raw_transactions(ingestion_run_id);

-- ---------------------------------------------------------------------------
-- target_matches: join table linking raw transactions to index targets
-- (P0-W1 Section 3.2)
-- ---------------------------------------------------------------------------

CREATE TABLE target_matches (
    id                  UUID PRIMARY KEY,
    target_id           UUID NOT NULL REFERENCES index_targets(id),
    raw_transaction_id  UUID NOT NULL REFERENCES raw_transactions(id),
    match_reason        TEXT,
    matched_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE target_matches
    ADD CONSTRAINT uq_target_matches_target_raw_tx UNIQUE (target_id, raw_transaction_id);

CREATE INDEX idx_target_matches_target ON target_matches(target_id);
CREATE INDEX idx_target_matches_raw_tx ON target_matches(raw_transaction_id);

-- ---------------------------------------------------------------------------
-- ingestion_runs: control-plane record for ingestion operations
-- (P0-W1 Section 3.4)
-- ---------------------------------------------------------------------------

CREATE TABLE ingestion_runs (
    id              UUID PRIMARY KEY,
    target_id       UUID REFERENCES index_targets(id),
    network         TEXT NOT NULL,
    source          TEXT NOT NULL,
    mode            TEXT NOT NULL,
    status          TEXT NOT NULL,
    started_at      TIMESTAMPTZ NOT NULL,
    finished_at     TIMESTAMPTZ,
    records_written BIGINT NOT NULL DEFAULT 0,
    error_message   TEXT,
    cursor_state    JSONB
);

CREATE INDEX idx_ingestion_runs_target ON ingestion_runs(target_id);
CREATE INDEX idx_ingestion_runs_status ON ingestion_runs(status);

-- ---------------------------------------------------------------------------
-- checkpoints: V2 resumption cursors keyed on (target, network, source)
-- (P0-W1 Section 3.4, P0-W3 Section 5)
-- ---------------------------------------------------------------------------

CREATE TABLE checkpoints (
    id          UUID PRIMARY KEY,
    target_id   UUID NOT NULL REFERENCES index_targets(id),
    network     TEXT NOT NULL,
    source      TEXT NOT NULL,
    cursor      JSONB NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE checkpoints
    ADD CONSTRAINT uq_checkpoints_target_network_source UNIQUE (target_id, network, source);

-- ---------------------------------------------------------------------------
-- dataset_versions: parser/materializer version tracking for safe reprocessing
-- (P0-W1 Section 3.5)
-- ---------------------------------------------------------------------------

CREATE TABLE dataset_versions (
    id           UUID PRIMARY KEY,
    dataset_name TEXT NOT NULL,
    version      INT NOT NULL,
    parser_hash  TEXT,
    created_at   TIMESTAMPTZ NOT NULL,
    notes        TEXT
);

-- Wire up the FK from raw_transactions.ingestion_run_id now that ingestion_runs exists.
ALTER TABLE raw_transactions
    ADD CONSTRAINT fk_raw_transactions_ingestion_run
    FOREIGN KEY (ingestion_run_id) REFERENCES ingestion_runs(id);
