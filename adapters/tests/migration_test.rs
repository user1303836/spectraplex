//! P1-W5: Migration integration test suite.
//!
//! Verifies V2 schema correctness on fresh and upgraded databases:
//! - All 10 migrations apply cleanly (7 V1 + 3 V2)
//! - Uniqueness constraints reject duplicates
//! - Foreign keys reject orphan records
//! - Multi-target linkage for a single raw transaction
//! - V1 data survives V2 migration without loss
//! - Network seed data is correct
//! - V2 enum types match Rust enum variants
//!
//! Requires a local PostgreSQL instance (see CLAUDE.md for setup).
//! Set `TEST_DATABASE_URL` to a connectable Postgres URL (the test creates and
//! drops ephemeral databases automatically).

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, Executor, PgConnection, Row};
use std::str::FromStr;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helper: ephemeral database per test
// ---------------------------------------------------------------------------

/// Returns the base Postgres URL used to create/drop test databases.
/// Defaults to `postgres://localhost/postgres` if `TEST_DATABASE_URL` is unset.
fn base_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/postgres".to_string())
}

/// Returns true if PostgreSQL is reachable at the base URL.
/// Uses a 2-second timeout to avoid long hangs in CI without a service container.
async fn pg_is_available() -> bool {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        PgConnection::connect(&base_url()),
    )
    .await;
    match result {
        Ok(Ok(conn)) => {
            drop(conn);
            true
        }
        _ => false,
    }
}

/// Skip guard: call at the top of each test. Returns early if PG is unreachable.
macro_rules! require_pg {
    () => {
        if !pg_is_available().await {
            eprintln!(
                "SKIPPED: PostgreSQL not available at {} — set TEST_DATABASE_URL or start PostgreSQL",
                base_url()
            );
            return;
        }
    };
}

/// Create an ephemeral database with a unique name and return a pool connected
/// to it. The caller must call `drop_test_db` when done.
async fn create_test_db(prefix: &str) -> (PgPool, String) {
    let db_name = format!("spx_test_{}_{}", prefix, Uuid::new_v4().simple());

    // Connect to the maintenance database to CREATE the test database.
    let mut conn = PgConnection::connect(&base_url())
        .await
        .expect("Cannot connect to base Postgres URL — is PostgreSQL running?");

    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("Failed to create test database {db_name}: {e}"));

    conn.close().await.ok();

    // Build a connection URL pointing at the new database.
    let opts = PgConnectOptions::from_str(&base_url())
        .expect("bad base url")
        .database(&db_name)
        .log_statements(log::LevelFilter::Off);

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .unwrap_or_else(|e| panic!("Failed to connect to test database {db_name}: {e}"));

    (pool, db_name)
}

/// Drop the ephemeral test database.
async fn drop_test_db(db_name: &str) {
    let mut conn = PgConnection::connect(&base_url()).await.ok();
    if let Some(ref mut c) = conn {
        // Force-disconnect other sessions before dropping.
        let _ = sqlx::query(&format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
        ))
        .execute(&mut *c)
        .await;

        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{db_name}\""))
            .execute(&mut *c)
            .await;
    }
}

/// Apply all 10 migrations to the given pool.
async fn run_all_migrations(pool: &PgPool) {
    sqlx::migrate!("../migrations")
        .run(pool)
        .await
        .expect("Migrations failed");
}

/// Apply only the first `n` migrations, with tracking via `_sqlx_migrations`.
/// Successive calls continue from where the previous left off.
async fn run_n_migrations(pool: &PgPool, n: usize) {
    // Ensure the tracking table exists.
    pool.execute(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            success BOOLEAN NOT NULL,
            checksum BYTEA NOT NULL,
            execution_time BIGINT NOT NULL
        )",
    )
    .await
    .unwrap();

    let migrator = sqlx::migrate!("../migrations");
    for m in migrator.iter().take(n) {
        // Skip if already applied.
        let already: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = $1)")
                .bind(m.version)
                .fetch_one(pool)
                .await
                .unwrap();
        if already {
            continue;
        }

        pool.execute(m.sql.as_ref())
            .await
            .unwrap_or_else(|e| panic!("Migration {} failed: {e}", m.description));

        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
             VALUES ($1, $2, TRUE, $3, 0)",
        )
        .bind(m.version)
        .bind(&*m.description)
        .bind(&*m.checksum)
        .execute(pool)
        .await
        .unwrap();
    }
}

// ---------------------------------------------------------------------------
// Helpers: table/column introspection via information_schema
// ---------------------------------------------------------------------------

async fn table_exists(pool: &PgPool, table: &str) -> bool {
    let row = sqlx::query(
        "SELECT EXISTS (
            SELECT 1 FROM information_schema.tables
            WHERE table_schema = 'public' AND table_name = $1
        ) AS exists",
    )
    .bind(table)
    .fetch_one(pool)
    .await
    .unwrap();
    row.get::<bool, _>("exists")
}

async fn column_exists(pool: &PgPool, table: &str, column: &str) -> bool {
    let row = sqlx::query(
        "SELECT EXISTS (
            SELECT 1 FROM information_schema.columns
            WHERE table_schema = 'public' AND table_name = $1 AND column_name = $2
        ) AS exists",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .unwrap();
    row.get::<bool, _>("exists")
}

async fn enum_values(pool: &PgPool, enum_name: &str) -> Vec<String> {
    let rows = sqlx::query(
        "SELECT e.enumlabel AS label
         FROM pg_type t
         JOIN pg_enum e ON t.oid = e.enumtypid
         WHERE t.typname = $1
         ORDER BY e.enumsortorder",
    )
    .bind(enum_name)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.iter().map(|r| r.get::<String, _>("label")).collect()
}

async fn row_count(pool: &PgPool, table: &str) -> i64 {
    let row = sqlx::query(&format!("SELECT COUNT(*) AS cnt FROM \"{table}\""))
        .fetch_one(pool)
        .await
        .unwrap();
    row.get::<i64, _>("cnt")
}

// ============================================================================
// Test: Fresh-DB path — all 10 migrations on an empty database
// ============================================================================

#[tokio::test]
async fn fresh_db_all_tables_exist() {
    require_pg!();
    let (pool, db_name) = create_test_db("fresh").await;
    run_all_migrations(&pool).await;

    // V1 tables
    assert!(table_exists(&pool, "transactions").await, "transactions");
    assert!(
        table_exists(&pool, "ledger_entries").await,
        "ledger_entries"
    );
    assert!(table_exists(&pool, "blocks").await, "blocks");
    assert!(table_exists(&pool, "evm_logs").await, "evm_logs");
    assert!(table_exists(&pool, "hl_fills").await, "hl_fills");
    assert!(
        table_exists(&pool, "indexer_checkpoints").await,
        "indexer_checkpoints"
    );

    // V2 tables
    assert!(table_exists(&pool, "networks").await, "networks");
    assert!(table_exists(&pool, "index_targets").await, "index_targets");
    assert!(
        table_exists(&pool, "raw_transactions").await,
        "raw_transactions"
    );
    assert!(
        table_exists(&pool, "target_matches").await,
        "target_matches"
    );
    assert!(
        table_exists(&pool, "ingestion_runs").await,
        "ingestion_runs"
    );
    assert!(table_exists(&pool, "checkpoints").await, "checkpoints");
    assert!(
        table_exists(&pool, "dataset_versions").await,
        "dataset_versions"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn fresh_db_v2_table_columns() {
    require_pg!();
    let (pool, db_name) = create_test_db("cols").await;
    run_all_migrations(&pool).await;

    // index_targets
    for col in [
        "id",
        "kind",
        "network",
        "chain_family",
        "address",
        "filter_spec",
        "filter_spec_hash",
        "mode",
        "label",
        "owner_id",
        "created_at",
        "updated_at",
    ] {
        assert!(
            column_exists(&pool, "index_targets", col).await,
            "index_targets.{col} missing"
        );
    }

    // raw_transactions
    for col in [
        "id",
        "network",
        "tx_hash",
        "timestamp",
        "block_number",
        "raw_metadata",
        "source",
        "ingestion_run_id",
        "ingested_at",
    ] {
        assert!(
            column_exists(&pool, "raw_transactions", col).await,
            "raw_transactions.{col} missing"
        );
    }

    // target_matches
    for col in [
        "id",
        "target_id",
        "raw_transaction_id",
        "match_reason",
        "matched_at",
    ] {
        assert!(
            column_exists(&pool, "target_matches", col).await,
            "target_matches.{col} missing"
        );
    }

    // ingestion_runs
    for col in [
        "id",
        "target_id",
        "network",
        "source",
        "mode",
        "status",
        "started_at",
        "finished_at",
        "records_written",
        "error_message",
        "cursor_state",
    ] {
        assert!(
            column_exists(&pool, "ingestion_runs", col).await,
            "ingestion_runs.{col} missing"
        );
    }

    // checkpoints
    for col in [
        "id",
        "target_id",
        "network",
        "source",
        "cursor",
        "updated_at",
    ] {
        assert!(
            column_exists(&pool, "checkpoints", col).await,
            "checkpoints.{col} missing"
        );
    }

    // dataset_versions
    for col in [
        "id",
        "dataset_name",
        "version",
        "parser_hash",
        "created_at",
        "notes",
    ] {
        assert!(
            column_exists(&pool, "dataset_versions", col).await,
            "dataset_versions.{col} missing"
        );
    }

    // networks
    for col in [
        "id",
        "chain_family",
        "display_name",
        "is_testnet",
        "finality_model",
        "block_time_ms",
        "created_at",
    ] {
        assert!(
            column_exists(&pool, "networks", col).await,
            "networks.{col} missing"
        );
    }

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: Upgraded-DB path — V1 migrations, insert V1 data, then V2 migrations
// ============================================================================

#[tokio::test]
async fn upgraded_db_v1_data_survives_v2_migration() {
    require_pg!();
    let (pool, db_name) = create_test_db("upgrade").await;

    // Apply 7 V1 migrations
    run_n_migrations(&pool, 7).await;

    // Insert V1 data
    let tx_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO transactions (id, user_id, wallet_address, timestamp, tx_hash, chain, raw_metadata)
         VALUES ($1, $2, 'SoLwALLet1', 1700000000, 'v1txhash001', 'solana', '{\"slot\":100}'::jsonb)",
    )
    .bind(tx_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    let le_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO ledger_entries (id, transaction_id, user_id, wallet_address, asset_symbol, amount, entry_type)
         VALUES ($1, $2, $3, 'SoLwALLet1', 'SOL', 1.5, 'transfer')",
    )
    .bind(le_id)
    .bind(tx_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO indexer_checkpoints (chain, wallet_address, last_signature, last_slot, last_timestamp, last_block)
         VALUES ('solana', 'SoLwALLet1', 'sig123', 100, 1700000000, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Now apply V2 migrations (3)
    run_n_migrations(&pool, 10).await;

    // Verify V1 data is intact
    let v1_tx = sqlx::query("SELECT tx_hash FROM transactions WHERE id = $1")
        .bind(tx_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(v1_tx.get::<String, _>("tx_hash"), "v1txhash001");

    let v1_le = sqlx::query("SELECT asset_symbol, amount FROM ledger_entries WHERE id = $1")
        .bind(le_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(v1_le.get::<String, _>("asset_symbol"), "SOL");

    let v1_cp = sqlx::query("SELECT last_signature FROM indexer_checkpoints WHERE chain = 'solana' AND wallet_address = 'SoLwALLet1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        v1_cp.get::<Option<String>, _>("last_signature"),
        Some("sig123".to_string())
    );

    // Verify V2 tables are available after upgrade
    assert!(table_exists(&pool, "networks").await);
    assert!(table_exists(&pool, "index_targets").await);
    assert!(table_exists(&pool, "raw_transactions").await);
    assert!(table_exists(&pool, "target_matches").await);
    assert!(table_exists(&pool, "ingestion_runs").await);
    assert!(table_exists(&pool, "checkpoints").await);
    assert!(table_exists(&pool, "dataset_versions").await);

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: Network seed data — 13 canonical networks
// ============================================================================

#[tokio::test]
async fn network_seed_data_13_networks() {
    require_pg!();
    let (pool, db_name) = create_test_db("nets").await;
    run_all_migrations(&pool).await;

    assert_eq!(row_count(&pool, "networks").await, 13);

    // Verify specific networks
    let expected = [
        (
            "solana-mainnet",
            "solana",
            false,
            "probabilistic-slot",
            Some(400),
        ),
        (
            "solana-devnet",
            "solana",
            true,
            "probabilistic-slot",
            Some(400),
        ),
        (
            "solana-testnet",
            "solana",
            true,
            "probabilistic-slot",
            Some(400),
        ),
        (
            "ethereum-mainnet",
            "evm",
            false,
            "probabilistic-block",
            Some(12000),
        ),
        (
            "ethereum-sepolia",
            "evm",
            true,
            "probabilistic-block",
            Some(12000),
        ),
        (
            "base-mainnet",
            "evm",
            false,
            "deterministic-block",
            Some(2000),
        ),
        (
            "base-sepolia",
            "evm",
            true,
            "deterministic-block",
            Some(2000),
        ),
        (
            "arbitrum-mainnet",
            "evm",
            false,
            "deterministic-block",
            Some(250),
        ),
        (
            "arbitrum-sepolia",
            "evm",
            true,
            "deterministic-block",
            Some(250),
        ),
        (
            "hyperevm-mainnet",
            "evm",
            false,
            "deterministic-block",
            Some(2000),
        ),
        (
            "hyperevm-testnet",
            "evm",
            true,
            "deterministic-block",
            Some(2000),
        ),
        ("hypercore-mainnet", "hyperliquid", false, "instant", None),
        ("hypercore-testnet", "hyperliquid", true, "instant", None),
    ];

    for (id, family, testnet, finality, block_time) in expected {
        let row = sqlx::query(
            "SELECT chain_family::text, is_testnet, finality_model, block_time_ms FROM networks WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&pool)
        .await
        .unwrap_or_else(|e| panic!("Query failed for network {id}: {e}"));

        let row = row.unwrap_or_else(|| panic!("Network {id} not found"));
        assert_eq!(row.get::<String, _>("chain_family"), family, "{id} family");
        assert_eq!(row.get::<bool, _>("is_testnet"), testnet, "{id} testnet");
        assert_eq!(
            row.get::<String, _>("finality_model"),
            finality,
            "{id} finality"
        );
        assert_eq!(
            row.get::<Option<i32>, _>("block_time_ms"),
            block_time,
            "{id} block_time_ms"
        );
    }

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: V2 enum types match Rust enum variants
// ============================================================================

#[tokio::test]
async fn v2_enum_chain_family_values() {
    require_pg!();
    let (pool, db_name) = create_test_db("enumcf").await;
    run_all_migrations(&pool).await;

    let vals = enum_values(&pool, "chain_family_enum").await;
    assert_eq!(vals, vec!["solana", "evm", "hyperliquid"]);

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn v2_enum_target_kind_values() {
    require_pg!();
    let (pool, db_name) = create_test_db("enumtk").await;
    run_all_migrations(&pool).await;

    let vals = enum_values(&pool, "target_kind_enum").await;
    assert_eq!(
        vals,
        vec![
            "wallet",
            "contract",
            "program",
            "account",
            "topic_filter",
            "market",
            "pool",
            "protocol"
        ]
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn v2_enum_target_mode_values() {
    require_pg!();
    let (pool, db_name) = create_test_db("enumtm").await;
    run_all_migrations(&pool).await;

    let vals = enum_values(&pool, "target_mode_enum").await;
    assert_eq!(vals, vec!["backfill", "stream", "both"]);

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: Uniqueness constraints
// ============================================================================

/// Helper: insert an index_target and return its id.
async fn insert_target(pool: &PgPool, kind: &str, network: &str, address: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, address, mode, created_at, updated_at)
         VALUES ($1, $2::target_kind_enum, $3, 'solana'::chain_family_enum, $4, 'both'::target_mode_enum, NOW(), NOW())",
    )
    .bind(id)
    .bind(kind)
    .bind(network)
    .bind(address)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Helper: insert an index_target with a specific owner and return its id.
async fn insert_target_with_owner(
    pool: &PgPool,
    kind: &str,
    network: &str,
    address: &str,
    owner: Uuid,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, address, mode, owner_id, created_at, updated_at)
         VALUES ($1, $2::target_kind_enum, $3, 'solana'::chain_family_enum, $4, 'both'::target_mode_enum, $5, NOW(), NOW())",
    )
    .bind(id)
    .bind(kind)
    .bind(network)
    .bind(address)
    .bind(owner)
    .execute(pool)
    .await
    .unwrap();
    id
}
async fn insert_raw_tx(pool: &PgPool, network: &str, tx_hash: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO raw_transactions (id, network, tx_hash, timestamp, raw_metadata, source, ingested_at)
         VALUES ($1, $2, $3, 1700000000, '{}'::jsonb, 'rpc', NOW())",
    )
    .bind(id)
    .bind(network)
    .bind(tx_hash)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Helper: insert a target_match and return its id.
async fn insert_match(pool: &PgPool, target_id: Uuid, raw_tx_id: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO target_matches (id, target_id, raw_transaction_id, matched_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind(id)
    .bind(target_id)
    .bind(raw_tx_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn uniqueness_index_targets_address() {
    require_pg!();
    let (pool, db_name) = create_test_db("uqaddr").await;
    run_all_migrations(&pool).await;

    let owner = Uuid::new_v4();

    insert_target_with_owner(&pool, "wallet", "solana-mainnet", "WaLLetABC", owner).await;

    // Duplicate (kind, network, address, owner_id) must fail.
    let dup = sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, address, mode, owner_id, created_at, updated_at)
         VALUES ($1, 'wallet'::target_kind_enum, 'solana-mainnet', 'solana'::chain_family_enum, 'WaLLetABC', 'both'::target_mode_enum, $2, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .execute(&pool)
    .await;
    assert!(
        dup.is_err(),
        "uq_index_targets_address_owner should reject duplicate for same owner"
    );

    // Different owner for same kind/network/address is allowed (tenant isolation).
    let other_owner = Uuid::new_v4();
    let ok = sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, address, mode, owner_id, created_at, updated_at)
         VALUES ($1, 'wallet'::target_kind_enum, 'solana-mainnet', 'solana'::chain_family_enum, 'WaLLetABC', 'both'::target_mode_enum, $2, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(other_owner)
    .execute(&pool)
    .await;
    assert!(
        ok.is_ok(),
        "different owner should be allowed for same network+address"
    );

    // Different kind for same network+address+owner is allowed.
    let ok2 = sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, address, mode, owner_id, created_at, updated_at)
         VALUES ($1, 'account'::target_kind_enum, 'solana-mainnet', 'solana'::chain_family_enum, 'WaLLetABC', 'both'::target_mode_enum, $2, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(owner)
    .execute(&pool)
    .await;
    assert!(
        ok2.is_ok(),
        "different kind should be allowed for same network+address+owner"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn uniqueness_index_targets_filter_spec() {
    require_pg!();
    let (pool, db_name) = create_test_db("uqfspec").await;
    run_all_migrations(&pool).await;

    // Insert an index_target with a non-null filter_spec_hash.
    sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, filter_spec, filter_spec_hash, mode, created_at, updated_at)
         VALUES ($1, 'topic_filter'::target_kind_enum, 'solana-mainnet', 'solana'::chain_family_enum, '{\"program\":\"11111111111111111111111111111111\"}'::jsonb, 'abc123hash', 'both'::target_mode_enum, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();

    // Duplicate (kind, network, filter_spec_hash) must fail.
    let dup = sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, filter_spec, filter_spec_hash, mode, created_at, updated_at)
         VALUES ($1, 'topic_filter'::target_kind_enum, 'solana-mainnet', 'solana'::chain_family_enum, '{\"program\":\"different\"}'::jsonb, 'abc123hash', 'both'::target_mode_enum, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert!(
        dup.is_err(),
        "uq_index_targets_filter_spec should reject duplicate (kind, network, filter_spec_hash)"
    );

    // NULL filter_spec_hash rows are exempt — multiple NULLs allowed.
    let null1 = sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, address, mode, created_at, updated_at)
         VALUES ($1, 'topic_filter'::target_kind_enum, 'solana-mainnet', 'solana'::chain_family_enum, 'NullFSAddr1', 'both'::target_mode_enum, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert!(null1.is_ok(), "NULL filter_spec_hash should be exempt (1)");

    let null2 = sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, address, mode, created_at, updated_at)
         VALUES ($1, 'topic_filter'::target_kind_enum, 'solana-mainnet', 'solana'::chain_family_enum, 'NullFSAddr2', 'both'::target_mode_enum, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert!(null2.is_ok(), "NULL filter_spec_hash should be exempt (2)");

    // Different kind with same (network, filter_spec_hash) is allowed.
    let diff_kind = sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, filter_spec, filter_spec_hash, mode, created_at, updated_at)
         VALUES ($1, 'protocol'::target_kind_enum, 'solana-mainnet', 'solana'::chain_family_enum, '{\"program\":\"other\"}'::jsonb, 'abc123hash', 'both'::target_mode_enum, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert!(
        diff_kind.is_ok(),
        "different kind should be allowed for same (network, filter_spec_hash)"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn uniqueness_raw_transactions_network_tx_hash() {
    require_pg!();
    let (pool, db_name) = create_test_db("uqtx").await;
    run_all_migrations(&pool).await;

    insert_raw_tx(&pool, "solana-mainnet", "txhash_unique_1").await;

    // Duplicate (network, tx_hash) must fail.
    let dup = sqlx::query(
        "INSERT INTO raw_transactions (id, network, tx_hash, timestamp, raw_metadata, source, ingested_at)
         VALUES ($1, 'solana-mainnet', 'txhash_unique_1', 1700000001, '{}'::jsonb, 'rpc', NOW())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert!(
        dup.is_err(),
        "uq_raw_transactions_network_tx_hash should reject duplicate"
    );

    // Same tx_hash on a different network is allowed.
    let ok = insert_raw_tx(&pool, "ethereum-mainnet", "txhash_unique_1").await;
    assert_ne!(ok, Uuid::nil());

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn uniqueness_target_matches_target_raw_tx() {
    require_pg!();
    let (pool, db_name) = create_test_db("uqmatch").await;
    run_all_migrations(&pool).await;

    let target_id = insert_target(&pool, "wallet", "solana-mainnet", "MatchAddr1").await;
    let raw_tx_id = insert_raw_tx(&pool, "solana-mainnet", "match_tx_1").await;
    insert_match(&pool, target_id, raw_tx_id).await;

    // Duplicate (target_id, raw_transaction_id) must fail.
    let dup = sqlx::query(
        "INSERT INTO target_matches (id, target_id, raw_transaction_id, matched_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(target_id)
    .bind(raw_tx_id)
    .execute(&pool)
    .await;
    assert!(
        dup.is_err(),
        "uq_target_matches_target_raw_tx should reject duplicate"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn uniqueness_checkpoints_target_network_source() {
    require_pg!();
    let (pool, db_name) = create_test_db("uqcp").await;
    run_all_migrations(&pool).await;

    let target_id = insert_target(&pool, "wallet", "solana-mainnet", "CpAddr1").await;

    sqlx::query(
        "INSERT INTO checkpoints (id, target_id, network, source, cursor, updated_at)
         VALUES ($1, $2, 'solana-mainnet', 'rpc', '{\"slot\":1}'::jsonb, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(target_id)
    .execute(&pool)
    .await
    .unwrap();

    // Duplicate (target_id, network, source) must fail.
    let dup = sqlx::query(
        "INSERT INTO checkpoints (id, target_id, network, source, cursor, updated_at)
         VALUES ($1, $2, 'solana-mainnet', 'rpc', '{\"slot\":2}'::jsonb, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(target_id)
    .execute(&pool)
    .await;
    assert!(
        dup.is_err(),
        "uq_checkpoints_target_network_source should reject duplicate"
    );

    // Different source for same target+network is allowed.
    let ok = sqlx::query(
        "INSERT INTO checkpoints (id, target_id, network, source, cursor, updated_at)
         VALUES ($1, $2, 'solana-mainnet', 'grpc', '{\"slot\":1}'::jsonb, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(target_id)
    .execute(&pool)
    .await;
    assert!(ok.is_ok(), "different source should be allowed");

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: Foreign keys reject orphan records
// ============================================================================

#[tokio::test]
async fn fk_raw_transactions_network_references_networks() {
    require_pg!();
    let (pool, db_name) = create_test_db("fknet").await;
    run_all_migrations(&pool).await;

    // Insert with a bogus network must fail.
    let bad = sqlx::query(
        "INSERT INTO raw_transactions (id, network, tx_hash, timestamp, raw_metadata, source, ingested_at)
         VALUES ($1, 'bogus-network', 'fk_tx_1', 1700000000, '{}'::jsonb, 'rpc', NOW())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "FK on raw_transactions.network should reject bogus network"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn fk_target_matches_target_id_references_index_targets() {
    require_pg!();
    let (pool, db_name) = create_test_db("fktgt").await;
    run_all_migrations(&pool).await;

    let raw_tx_id = insert_raw_tx(&pool, "solana-mainnet", "fk_match_tx_1").await;

    // Insert with a bogus target_id must fail.
    let bad = sqlx::query(
        "INSERT INTO target_matches (id, target_id, raw_transaction_id, matched_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4()) // non-existent target
    .bind(raw_tx_id)
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "FK on target_matches.target_id should reject orphan"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn fk_target_matches_raw_transaction_id_references_raw_transactions() {
    require_pg!();
    let (pool, db_name) = create_test_db("fkrtx").await;
    run_all_migrations(&pool).await;

    let target_id = insert_target(&pool, "wallet", "solana-mainnet", "FKAddr1").await;

    // Insert with a bogus raw_transaction_id must fail.
    let bad = sqlx::query(
        "INSERT INTO target_matches (id, target_id, raw_transaction_id, matched_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(target_id)
    .bind(Uuid::new_v4()) // non-existent raw_tx
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "FK on target_matches.raw_transaction_id should reject orphan"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn fk_checkpoints_target_id_references_index_targets() {
    require_pg!();
    let (pool, db_name) = create_test_db("fkcp").await;
    run_all_migrations(&pool).await;

    let bad = sqlx::query(
        "INSERT INTO checkpoints (id, target_id, network, source, cursor, updated_at)
         VALUES ($1, $2, 'solana-mainnet', 'rpc', '{}'::jsonb, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4()) // non-existent target
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "FK on checkpoints.target_id should reject orphan"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn fk_ingestion_runs_target_id_references_index_targets() {
    require_pg!();
    let (pool, db_name) = create_test_db("fkrun").await;
    run_all_migrations(&pool).await;

    let bad = sqlx::query(
        "INSERT INTO ingestion_runs (id, target_id, network, source, mode, status, started_at, records_written)
         VALUES ($1, $2, 'solana-mainnet', 'rpc', 'backfill', 'running', NOW(), 0)",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4()) // non-existent target
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "FK on ingestion_runs.target_id should reject orphan"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

#[tokio::test]
async fn fk_raw_transactions_ingestion_run_id_references_ingestion_runs() {
    require_pg!();
    let (pool, db_name) = create_test_db("fkirun").await;
    run_all_migrations(&pool).await;

    let bad = sqlx::query(
        "INSERT INTO raw_transactions (id, network, tx_hash, timestamp, raw_metadata, source, ingestion_run_id, ingested_at)
         VALUES ($1, 'solana-mainnet', 'fk_irun_tx', 1700000000, '{}'::jsonb, 'rpc', $2, NOW())",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4()) // non-existent ingestion_run
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "FK on raw_transactions.ingestion_run_id should reject orphan"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: Multi-target linkage — one raw_transaction, two targets
// ============================================================================

#[tokio::test]
async fn multi_target_linkage_single_raw_tx() {
    require_pg!();
    let (pool, db_name) = create_test_db("multi").await;
    run_all_migrations(&pool).await;

    // Create two index_targets (different addresses).
    let target_a = insert_target(&pool, "wallet", "solana-mainnet", "WalletA").await;
    let target_b = insert_target(&pool, "wallet", "solana-mainnet", "WalletB").await;

    // Create one raw_transaction.
    let raw_tx = insert_raw_tx(&pool, "solana-mainnet", "multi_target_tx_1").await;

    // Link both targets to the same raw_transaction.
    insert_match(&pool, target_a, raw_tx).await;
    insert_match(&pool, target_b, raw_tx).await;

    // Query matches for the raw_transaction: should return 2.
    let matches = sqlx::query(
        "SELECT target_id FROM target_matches WHERE raw_transaction_id = $1 ORDER BY target_id",
    )
    .bind(raw_tx)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        matches.len(),
        2,
        "expected 2 target_matches for the same raw_tx"
    );

    let match_targets: Vec<Uuid> = matches.iter().map(|r| r.get("target_id")).collect();
    assert!(match_targets.contains(&target_a));
    assert!(match_targets.contains(&target_b));

    // Query by target_a should find the raw_tx.
    let by_a = sqlx::query("SELECT raw_transaction_id FROM target_matches WHERE target_id = $1")
        .bind(target_a)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(by_a.len(), 1);
    assert_eq!(by_a[0].get::<Uuid, _>("raw_transaction_id"), raw_tx);

    // Query by target_b should also find the raw_tx.
    let by_b = sqlx::query("SELECT raw_transaction_id FROM target_matches WHERE target_id = $1")
        .bind(target_b)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(by_b.len(), 1);
    assert_eq!(by_b[0].get::<Uuid, _>("raw_transaction_id"), raw_tx);

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: V1+V2 coexistence — both table sets are independently usable
// ============================================================================

#[tokio::test]
async fn v1_v2_coexistence() {
    require_pg!();
    let (pool, db_name) = create_test_db("coexist").await;
    run_all_migrations(&pool).await;

    // Insert V1 data.
    let v1_tx_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO transactions (id, user_id, wallet_address, timestamp, tx_hash, chain, raw_metadata)
         VALUES ($1, $2, 'CoexistWallet', 1700000000, 'coexist_tx_1', 'solana', '{\"slot\":50}'::jsonb)",
    )
    .bind(v1_tx_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .unwrap();

    // Insert V2 data.
    let target_id = insert_target(&pool, "wallet", "solana-mainnet", "CoexistWallet").await;
    let raw_tx_id = insert_raw_tx(&pool, "solana-mainnet", "coexist_v2_tx").await;
    insert_match(&pool, target_id, raw_tx_id).await;

    // Both queries work.
    let v1_count = row_count(&pool, "transactions").await;
    assert!(v1_count >= 1, "V1 transactions should exist");

    let v2_count = row_count(&pool, "raw_transactions").await;
    assert!(v2_count >= 1, "V2 raw_transactions should exist");

    let match_count = row_count(&pool, "target_matches").await;
    assert!(match_count >= 1, "V2 target_matches should exist");

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: V1 enum types still present
// ============================================================================

#[tokio::test]
async fn v1_enums_preserved() {
    require_pg!();
    let (pool, db_name) = create_test_db("v1enum").await;
    run_all_migrations(&pool).await;

    let chain_vals = enum_values(&pool, "chain_enum").await;
    assert_eq!(chain_vals, vec!["solana", "hyperliquid", "ethereum"]);

    let entry_vals = enum_values(&pool, "entry_type_enum").await;
    assert_eq!(
        entry_vals,
        vec!["trade", "fee", "transfer", "staking", "income"]
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// Test: index_targets network FK references networks table
// ============================================================================

#[tokio::test]
async fn fk_index_targets_network_references_networks() {
    require_pg!();
    let (pool, db_name) = create_test_db("fkitnet").await;
    run_all_migrations(&pool).await;

    let bad = sqlx::query(
        "INSERT INTO index_targets (id, kind, network, chain_family, address, mode, created_at, updated_at)
         VALUES ($1, 'wallet'::target_kind_enum, 'nonexistent-network', 'solana'::chain_family_enum, 'Addr1', 'both'::target_mode_enum, NOW(), NOW())",
    )
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await;
    assert!(
        bad.is_err(),
        "FK on index_targets.network should reject bogus network"
    );

    pool.close().await;
    drop_test_db(&db_name).await;
}
