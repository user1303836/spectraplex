//! P3: Minimal operational hardening tests.
//!
//! Integration tests covering idempotency, tenant isolation, and export
//! lifecycle. Requires a local PostgreSQL instance.
//! Set `TEST_DATABASE_URL` to a connectable Postgres URL.

use chrono::Utc;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, PgConnection, Row};
use std::str::FromStr;
use uuid::Uuid;

use spectraplex_adapters::repo::Repository;

use spectraplex_core::materializer::{DatasetName, ExportFormat};
use spectraplex_core::models::{Chain, Transaction};
use spectraplex_core::v2::{ChainFamily, IndexTarget, TargetKind, TargetMode};

// ---------------------------------------------------------------------------
// Helper: ephemeral database per test (reused from ingestion_compat_test.rs)
// ---------------------------------------------------------------------------

fn base_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://localhost/postgres".to_string())
}

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

async fn create_test_db(prefix: &str) -> (PgPool, String) {
    let db_name = format!("spx_hard_{}_{}", prefix, Uuid::new_v4().simple());

    let mut conn = PgConnection::connect(&base_url())
        .await
        .expect("Cannot connect to base Postgres URL — is PostgreSQL running?");

    sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
        .execute(&mut conn)
        .await
        .unwrap_or_else(|e| panic!("Failed to create test database {db_name}: {e}"));

    conn.close().await.ok();

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

async fn drop_test_db(db_name: &str) {
    let mut conn = PgConnection::connect(&base_url()).await.ok();
    if let Some(ref mut c) = conn {
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

async fn run_all_migrations(pool: &PgPool) {
    sqlx::migrate!("../migrations")
        .run(pool)
        .await
        .expect("Migrations failed");
}

async fn setup_test_repo(prefix: &str) -> (Repository, PgPool, String) {
    let (pool, db_name) = create_test_db(prefix).await;
    run_all_migrations(&pool).await;
    let repo = Repository::new(pool.clone());
    (repo, pool, db_name)
}

fn make_target(
    kind: TargetKind,
    chain_family: ChainFamily,
    network: &str,
    address: Option<&str>,
    owner_id: Option<Uuid>,
) -> IndexTarget {
    let now = Utc::now();
    IndexTarget {
        id: Uuid::new_v4(),
        kind,
        network: network.to_string(),
        chain_family,
        address: address.map(|s| s.to_string()),
        filter_spec: None,
        mode: TargetMode::Backfill,
        label: None,
        owner_id,
        created_at: now,
        updated_at: now,
    }
}

fn make_v1_tx(chain: Chain, tx_hash: &str, wallet: &str) -> Transaction {
    Transaction {
        id: Uuid::new_v4(),
        user_id: Uuid::new_v4(),
        wallet_address: wallet.to_string(),
        timestamp: 1700000000,
        tx_hash: tx_hash.to_string(),
        chain,
        raw_metadata: serde_json::json!({"slot": 100}),
    }
}

// ============================================================================
// 1. Idempotency: Silver materialization is idempotent for duplicate runs
// ============================================================================

#[tokio::test]
async fn silver_materialization_is_idempotent() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("idempotency").await;

    // Create a target
    let target = make_target(
        TargetKind::Wallet,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"),
        None,
    );
    repo.create_index_target(&target).await.unwrap();

    // Ingest V1 transactions
    let txs = vec![
        make_v1_tx(
            Chain::Solana,
            "tx1",
            "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
        ),
        make_v1_tx(
            Chain::Solana,
            "tx2",
            "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
        ),
    ];
    repo.save_transactions(&txs).await.unwrap();

    // First materialization
    let r1 = repo
        .materialize_silver_datasets(&txs, Some("solana-mainnet"))
        .await;
    assert!(r1.all_succeeded(), "first materialization should succeed");
    let count1 = row_count(&_pool, "token_transfers").await;

    // Second materialization with same transactions
    let r2 = repo
        .materialize_silver_datasets(&txs, Some("solana-mainnet"))
        .await;
    assert!(r2.all_succeeded(), "second materialization should succeed");
    let count2 = row_count(&_pool, "token_transfers").await;

    // Row count should not increase — upserts are idempotent
    assert_eq!(
        count1, count2,
        "duplicate materialization should not create new rows"
    );

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 2. Tenant isolation: owner-scoped targets cannot cross query
// ============================================================================

#[tokio::test]
async fn tenant_isolation_prevents_cross_target_access() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("tenant_iso").await;

    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();

    // Tenant A registers a target
    let target_a = make_target(
        TargetKind::Wallet,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("addr_a"),
        Some(tenant_a),
    );
    repo.create_index_target(&target_a).await.unwrap();

    // Tenant B registers a target
    let target_b = make_target(
        TargetKind::Wallet,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("addr_b"),
        Some(tenant_b),
    );
    repo.create_index_target(&target_b).await.unwrap();

    // Tenant A lists their targets — should see only target_a
    let a_targets = repo
        .list_index_targets_by_owner(tenant_a, None, None)
        .await
        .unwrap();
    assert_eq!(a_targets.len(), 1);
    assert_eq!(a_targets[0].id, target_a.id);

    // Tenant B lists their targets — should see only target_b
    let b_targets = repo
        .list_index_targets_by_owner(tenant_b, None, None)
        .await
        .unwrap();
    assert_eq!(b_targets.len(), 1);
    assert_eq!(b_targets[0].id, target_b.id);

    // Admin (no owner filter) sees both
    let admin_targets = repo.list_index_targets(100, 0).await.unwrap();
    assert_eq!(admin_targets.len(), 2);

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 3. Export lifecycle: enqueue -> claim -> complete for a Gold dataset
// ============================================================================

#[tokio::test]
async fn gold_export_job_lifecycle() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("export_life").await;

    // Enqueue an export job for a Gold dataset
    let job = repo
        .enqueue_export_job(
            DatasetName::WalletLedger,
            ExportFormat::Jsonl,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(job.dataset, DatasetName::WalletLedger);

    // Claim the job
    let claimed = repo.claim_export_job("worker-1").await.unwrap();
    assert!(claimed.is_some(), "job should be claimable");
    let claimed = claimed.unwrap();
    assert_eq!(claimed.id, job.id);

    // Update to completed
    repo.update_export_job_status(
        job.id,
        "completed",
        Some(42),
        Some("/exports/test.jsonl"),
        None,
        "worker-1",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // Verify final state
    let final_job = repo.get_export_job(job.id).await.unwrap().unwrap();
    assert_eq!(
        final_job.status,
        spectraplex_core::v2::ExportJobStatus::Completed
    );
    assert_eq!(final_job.record_count, Some(42));
    assert_eq!(
        final_job.result_location,
        Some("/exports/test.jsonl".to_string())
    );

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 4. Restart/reclaim: stale running job is reclaimable
// ============================================================================

#[tokio::test]
async fn stale_running_job_is_reclaimed() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("reclaim").await;

    // Enqueue and claim a job
    let job = repo
        .enqueue_export_job(
            DatasetName::BalanceHistory,
            ExportFormat::Csv,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    let _claimed = repo.claim_export_job("worker-dead").await.unwrap().unwrap();

    // Manually backdate heartbeat to make it stale
    sqlx::query(
        "UPDATE export_jobs SET heartbeat_at = NOW() - make_interval(mins => 10) WHERE id = $1",
    )
    .bind(job.id)
    .execute(&_pool)
    .await
    .unwrap();

    // A new worker should be able to reclaim it
    let reclaimed = repo.claim_export_job("worker-new").await.unwrap();
    assert!(reclaimed.is_some(), "stale job should be reclaimable");
    let reclaimed = reclaimed.unwrap();
    assert_eq!(reclaimed.id, job.id);
    assert_eq!(reclaimed.worker_id, Some("worker-new".to_string()));

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ---------------------------------------------------------------------------
// Helper: row count
// ---------------------------------------------------------------------------

async fn row_count(pool: &PgPool, table: &str) -> i64 {
    let row = sqlx::query(&format!("SELECT COUNT(*) AS cnt FROM \"{table}\""))
        .fetch_one(pool)
        .await
        .unwrap();
    row.get::<i64, _>("cnt")
}
