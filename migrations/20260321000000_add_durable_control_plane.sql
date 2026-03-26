-- Workstream C: Durable control plane tables.
--
-- Moves jobs, streams, and export state from in-memory RwLock<HashMap<...>>
-- in AppState to durable Postgres tables that survive process restarts.
--
-- Tables added:
--   ingestion_jobs        - queued/running ingestion work items
--   ingestion_job_attempts - worker claim and heartbeat tracking
--   stream_subscriptions  - durable stream lifecycle and lease state
--   export_jobs           - durable export job state
--   materialization_runs  - ETL run tracking with provenance

-- ---------------------------------------------------------------------------
-- 1. ingestion_jobs: desired ingestion work items
-- ---------------------------------------------------------------------------

CREATE TABLE ingestion_jobs (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_id        UUID REFERENCES index_targets(id) ON DELETE SET NULL,
    network          TEXT NOT NULL,
    mode             TEXT NOT NULL,
    status           TEXT NOT NULL DEFAULT 'pending',
    priority         INT NOT NULL DEFAULT 0,
    idempotency_key  TEXT,
    requested_by     TEXT,
    callback_url     TEXT,
    error_message    TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chk_ingestion_jobs_status CHECK (
        status IN ('pending', 'claimed', 'running', 'completed', 'failed', 'cancelled')
    ),
    CONSTRAINT chk_ingestion_jobs_mode CHECK (
        mode IN ('backfill', 'incremental')
    )
);

-- Idempotency: reject duplicate enqueue requests.
CREATE UNIQUE INDEX uq_ingestion_jobs_idempotency_key
    ON ingestion_jobs (idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- Worker claim queries: find pending jobs ordered by priority.
CREATE INDEX idx_ingestion_jobs_status_priority
    ON ingestion_jobs (status, priority DESC, created_at ASC);

CREATE INDEX idx_ingestion_jobs_target ON ingestion_jobs (target_id);

-- ---------------------------------------------------------------------------
-- 2. ingestion_job_attempts: worker claim and heartbeat tracking
-- ---------------------------------------------------------------------------

CREATE TABLE ingestion_job_attempts (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    job_id        UUID NOT NULL REFERENCES ingestion_jobs(id) ON DELETE CASCADE,
    worker_id     TEXT NOT NULL,
    attempt_num   INT NOT NULL DEFAULT 1,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at   TIMESTAMPTZ,
    heartbeat_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    error_message TEXT
);

CREATE INDEX idx_ingestion_job_attempts_job ON ingestion_job_attempts (job_id);
CREATE INDEX idx_ingestion_job_attempts_worker ON ingestion_job_attempts (worker_id);

-- ---------------------------------------------------------------------------
-- 3. stream_subscriptions: durable stream lifecycle and lease state
-- ---------------------------------------------------------------------------

CREATE TABLE stream_subscriptions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_id       UUID REFERENCES index_targets(id) ON DELETE SET NULL,
    network         TEXT NOT NULL,
    source          TEXT NOT NULL DEFAULT 'grpc',
    desired_status  TEXT NOT NULL DEFAULT 'active',
    actual_status   TEXT NOT NULL DEFAULT 'pending',
    lease_owner     TEXT,
    heartbeat_at    TIMESTAMPTZ,
    cursor_state    JSONB,
    config          JSONB,
    error_message   TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chk_stream_subs_desired_status CHECK (
        desired_status IN ('active', 'paused', 'stopped')
    ),
    CONSTRAINT chk_stream_subs_actual_status CHECK (
        actual_status IN ('pending', 'running', 'paused', 'stopped', 'error')
    ),
    CONSTRAINT chk_stream_subs_source CHECK (
        source IN ('grpc', 'ws', 'rpc')
    )
);

-- One active subscription per target+network+source.
CREATE UNIQUE INDEX uq_stream_subs_target_network_source
    ON stream_subscriptions (target_id, network, source)
    WHERE target_id IS NOT NULL;

CREATE INDEX idx_stream_subs_desired_status
    ON stream_subscriptions (desired_status);
CREATE INDEX idx_stream_subs_actual_status
    ON stream_subscriptions (actual_status);
CREATE INDEX idx_stream_subs_lease_owner
    ON stream_subscriptions (lease_owner)
    WHERE lease_owner IS NOT NULL;

-- ---------------------------------------------------------------------------
-- 4. export_jobs: durable export job state
-- ---------------------------------------------------------------------------

CREATE TABLE export_jobs (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    dataset          TEXT NOT NULL,
    format           TEXT NOT NULL DEFAULT 'json',
    filters          JSONB,
    sink_config      JSONB,
    status           TEXT NOT NULL DEFAULT 'pending',
    worker_id        TEXT,
    record_count     INT,
    result_location  TEXT,
    error_message    TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at       TIMESTAMPTZ,
    completed_at     TIMESTAMPTZ,

    CONSTRAINT chk_export_jobs_status CHECK (
        status IN ('pending', 'running', 'delivering', 'completed', 'failed')
    )
);

CREATE INDEX idx_export_jobs_status ON export_jobs (status, created_at ASC);

-- ---------------------------------------------------------------------------
-- 5. materialization_runs: ETL run tracking with provenance
-- ---------------------------------------------------------------------------

CREATE TABLE materialization_runs (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    dataset_name         TEXT NOT NULL,
    scope                JSONB,
    input_watermark      BIGINT,
    output_record_count  BIGINT NOT NULL DEFAULT 0,
    status               TEXT NOT NULL DEFAULT 'pending',
    dataset_version_id   UUID REFERENCES dataset_versions(id) ON DELETE SET NULL,
    worker_id            TEXT,
    started_at           TIMESTAMPTZ,
    finished_at          TIMESTAMPTZ,
    error_message        TEXT,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chk_materialization_runs_status CHECK (
        status IN ('pending', 'running', 'completed', 'failed')
    )
);

CREATE INDEX idx_materialization_runs_dataset ON materialization_runs (dataset_name);
CREATE INDEX idx_materialization_runs_status ON materialization_runs (status, created_at ASC);
CREATE INDEX idx_materialization_runs_dataset_version
    ON materialization_runs (dataset_version_id)
    WHERE dataset_version_id IS NOT NULL;
