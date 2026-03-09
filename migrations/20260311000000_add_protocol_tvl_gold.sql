-- P5-W3: Gold-tier protocol analytics and TVL tables
-- protocol_events: protocol-level events derived from Silver decoded_events
-- pool_snapshots: per-pool reserve snapshots derived from decoded_events + token_transfers

CREATE TABLE IF NOT EXISTS protocol_events (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    network             TEXT NOT NULL,
    protocol_address    TEXT NOT NULL,
    protocol_name       TEXT,
    event_type          TEXT NOT NULL,
    event_details       JSONB NOT NULL DEFAULT '{}',
    pool_address        TEXT,
    raw_event_id        UUID REFERENCES decoded_events(id),
    timestamp           BIGINT NOT NULL,
    dataset_version_id  UUID REFERENCES dataset_versions(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_protocol_events_protocol_address
    ON protocol_events (protocol_address);
CREATE INDEX IF NOT EXISTS idx_protocol_events_pool_address
    ON protocol_events (pool_address);
CREATE INDEX IF NOT EXISTS idx_protocol_events_network
    ON protocol_events (network);
CREATE INDEX IF NOT EXISTS idx_protocol_events_timestamp
    ON protocol_events (timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_protocol_events_event_type
    ON protocol_events (event_type);

CREATE TABLE IF NOT EXISTS pool_snapshots (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    network             TEXT NOT NULL,
    pool_address        TEXT NOT NULL,
    protocol_address    TEXT NOT NULL,
    protocol_name       TEXT,
    token0_address      TEXT NOT NULL,
    token0_symbol       TEXT,
    token1_address      TEXT NOT NULL,
    token1_symbol       TEXT,
    reserve0            NUMERIC NOT NULL DEFAULT 0,
    reserve1            NUMERIC NOT NULL DEFAULT 0,
    tvl_usd             NUMERIC,
    snapshot_timestamp   BIGINT NOT NULL,
    block_number        BIGINT,
    dataset_version_id  UUID REFERENCES dataset_versions(id),
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_pool_snapshots_pool_address
    ON pool_snapshots (pool_address);
CREATE INDEX IF NOT EXISTS idx_pool_snapshots_protocol_address
    ON pool_snapshots (protocol_address);
CREATE INDEX IF NOT EXISTS idx_pool_snapshots_network
    ON pool_snapshots (network);
CREATE INDEX IF NOT EXISTS idx_pool_snapshots_snapshot_timestamp
    ON pool_snapshots (snapshot_timestamp DESC);
