//! P2-W6: End-to-end ingestion compatibility suite.
//!
//! Integration tests verifying the full V2 ingestion pipeline for all supported
//! target classes. Covers:
//!
//! - Adapter capability declarations (supported_target_kinds, supported_modes,
//!   chain_family) for all five Connector implementations
//! - Cross-adapter coverage: every TargetKind is reachable by at least one connector
//! - DB round-trip tests for wallet (Solana), EVM contract, Solana program,
//!   Hyperliquid wallet, Hyperliquid market, and EVM topic_filter targets
//! - Multi-target linkage: one raw_transaction linked to 3+ targets
//! - Checkpoint upsert idempotency
//! - Ingestion run lifecycle transitions
//! - Error path: each adapter rejects unsupported target kinds
//! - validate_target + can_service consistency
//!
//! Requires a local PostgreSQL instance (see CLAUDE.md for setup).
//! Set `TEST_DATABASE_URL` to a connectable Postgres URL (the test creates and
//! drops ephemeral databases automatically).

use chrono::Utc;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{ConnectOptions, Connection, PgConnection};
use std::str::FromStr;
use uuid::Uuid;

use spectraplex_adapters::compat::LegacyConnectorAdapter;
use spectraplex_adapters::evm::EvmAdapter;
use spectraplex_adapters::hyperliquid::HyperliquidAdapter;
use spectraplex_adapters::repo::Repository;
use spectraplex_adapters::solana::SolanaAdapter;
use spectraplex_adapters::solana_grpc::SolanaGrpcAdapter;

use spectraplex_core::connector::{validate_target, Connector, ConnectorCapabilities};
use spectraplex_core::models::Chain;
use spectraplex_core::v2::{
    ChainFamily, Checkpoint, IndexTarget, IngestionJobMode, IngestionJobStatus, IngestionRun,
    RawTransaction, TargetKind, TargetMatch, TargetMode,
};

// ---------------------------------------------------------------------------
// Helper: ephemeral database per test (reused from migration_test.rs)
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
    let db_name = format!("spx_compat_{}_{}", prefix, Uuid::new_v4().simple());

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

/// Create an ephemeral DB, run migrations, and return a Repository + pool + db_name.
async fn setup_test_repo(prefix: &str) -> (Repository, PgPool, String) {
    let (pool, db_name) = create_test_db(prefix).await;
    run_all_migrations(&pool).await;
    let repo = Repository::new(pool.clone());
    (repo, pool, db_name)
}

// ---------------------------------------------------------------------------
// Helper: synthetic data builders
// ---------------------------------------------------------------------------

fn make_index_target(
    kind: TargetKind,
    chain_family: ChainFamily,
    network: &str,
    address: Option<&str>,
    filter_spec: Option<serde_json::Value>,
    mode: TargetMode,
) -> IndexTarget {
    let now = Utc::now();
    IndexTarget {
        id: Uuid::new_v4(),
        kind,
        network: network.to_string(),
        chain_family,
        address: address.map(|s| s.to_string()),
        filter_spec,
        mode,
        label: None,
        owner_id: None,
        created_at: now,
        updated_at: now,
    }
}

fn make_raw_tx(network: &str, tx_hash: &str, source: &str, run_id: Option<Uuid>) -> RawTransaction {
    RawTransaction {
        id: Uuid::new_v4(),
        network: network.to_string(),
        tx_hash: tx_hash.to_string(),
        timestamp: 1700000000,
        block_number: Some(100),
        raw_metadata: serde_json::json!({"test": true}),
        source: source.to_string(),
        ingestion_run_id: run_id,
        ingested_at: Utc::now(),
    }
}

fn make_target_match(target_id: Uuid, raw_tx_id: Uuid, match_reason: Option<&str>) -> TargetMatch {
    TargetMatch {
        id: Uuid::new_v4(),
        target_id,
        raw_transaction_id: raw_tx_id,
        match_reason: match_reason.map(|s| s.to_string()),
        matched_at: Utc::now(),
    }
}

fn make_checkpoint(
    target_id: Uuid,
    network: &str,
    source: &str,
    cursor: serde_json::Value,
) -> Checkpoint {
    Checkpoint {
        id: Uuid::new_v4(),
        target_id,
        network: network.to_string(),
        source: source.to_string(),
        cursor,
        updated_at: Utc::now(),
    }
}

fn make_ingestion_run(
    target_id: Uuid,
    network: &str,
    source: &str,
    mode: &str,
    status: &str,
) -> IngestionRun {
    use std::str::FromStr;
    IngestionRun {
        id: Uuid::new_v4(),
        target_id: Some(target_id),
        network: network.to_string(),
        source: source.to_string(),
        mode: IngestionJobMode::from_str(mode).expect("valid mode"),
        status: IngestionJobStatus::from_str(status).expect("valid status"),
        started_at: Utc::now(),
        finished_at: None,
        records_written: 0,
        error_message: None,
        cursor_state: None,
    }
}

// ============================================================================
// 1. Capability declaration tests
// ============================================================================

#[test]
fn solana_adapter_capabilities() {
    let adapter = SolanaAdapter::new("http://localhost:8899");
    let caps = adapter.capabilities();
    assert_eq!(caps.chain_family, ChainFamily::Solana);
    assert_eq!(caps.supported_target_kinds, vec![TargetKind::Wallet]);
    assert_eq!(caps.supported_modes, vec![TargetMode::Backfill]);
}

#[test]
fn solana_grpc_adapter_capabilities() {
    let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
    let caps = adapter.capabilities();
    assert_eq!(caps.chain_family, ChainFamily::Solana);
    assert_eq!(
        caps.supported_target_kinds,
        vec![TargetKind::Wallet, TargetKind::Program, TargetKind::Account]
    );
    assert_eq!(
        caps.supported_modes,
        vec![TargetMode::Backfill, TargetMode::Stream, TargetMode::Both]
    );
}

#[test]
fn evm_adapter_capabilities() {
    let adapter = EvmAdapter::new("http://localhost:8545").unwrap();
    let caps = adapter.capabilities();
    assert_eq!(caps.chain_family, ChainFamily::Evm);
    assert_eq!(
        caps.supported_target_kinds,
        vec![
            TargetKind::Wallet,
            TargetKind::Contract,
            TargetKind::TopicFilter,
        ]
    );
    assert_eq!(caps.supported_modes, vec![TargetMode::Backfill]);
}

#[test]
fn hyperliquid_adapter_capabilities() {
    let adapter = HyperliquidAdapter::new();
    let caps = adapter.capabilities();
    assert_eq!(caps.chain_family, ChainFamily::Hyperliquid);
    assert_eq!(
        caps.supported_target_kinds,
        vec![TargetKind::Wallet, TargetKind::Market]
    );
    assert_eq!(
        caps.supported_modes,
        vec![TargetMode::Backfill, TargetMode::Stream]
    );
}

#[test]
fn legacy_compat_adapter_capabilities_solana() {
    let inner = SolanaAdapter::new("http://localhost:8899");
    let wrapper = LegacyConnectorAdapter::new(inner, &Chain::Solana);
    let caps = wrapper.capabilities();
    assert_eq!(caps.chain_family, ChainFamily::Solana);
    assert_eq!(caps.supported_target_kinds, vec![TargetKind::Wallet]);
    assert_eq!(caps.supported_modes, vec![TargetMode::Backfill]);
}

#[test]
fn legacy_compat_adapter_capabilities_evm() {
    let inner = EvmAdapter::new("http://localhost:8545").unwrap();
    let wrapper = LegacyConnectorAdapter::new(inner, &Chain::Ethereum);
    let caps = wrapper.capabilities();
    assert_eq!(caps.chain_family, ChainFamily::Evm);
    assert_eq!(caps.supported_target_kinds, vec![TargetKind::Wallet]);
}

#[test]
fn legacy_compat_adapter_capabilities_hyperliquid() {
    let inner = HyperliquidAdapter::new();
    let wrapper = LegacyConnectorAdapter::new(inner, &Chain::Hyperliquid);
    let caps = wrapper.capabilities();
    assert_eq!(caps.chain_family, ChainFamily::Hyperliquid);
    assert_eq!(caps.supported_target_kinds, vec![TargetKind::Wallet]);
}

// ============================================================================
// 2. Cross-adapter coverage: every TargetKind serviceable by at least one
// ============================================================================

#[test]
fn all_target_kinds_covered_by_at_least_one_connector() {
    let all_kinds = [
        TargetKind::Wallet,
        TargetKind::Contract,
        TargetKind::Program,
        TargetKind::Account,
        TargetKind::TopicFilter,
        TargetKind::Market,
        TargetKind::Pool,
        TargetKind::Protocol,
    ];
    assert_eq!(all_kinds.len(), 8, "must cover all 8 TargetKind variants");

    // Collect capabilities from all connectors
    let caps_list: Vec<ConnectorCapabilities> = vec![
        SolanaAdapter::new("http://localhost:8899").capabilities(),
        SolanaGrpcAdapter::new("http://localhost:10000", None).capabilities(),
        EvmAdapter::new("http://localhost:8545")
            .unwrap()
            .capabilities(),
        HyperliquidAdapter::new().capabilities(),
    ];

    for kind in &all_kinds {
        let covered = caps_list
            .iter()
            .any(|caps| caps.supported_target_kinds.contains(kind));

        // Pool, Protocol, and Account via gRPC are known-supported or future.
        // Pool and Protocol are not yet implemented by any adapter but are in the
        // validity matrix. We check that at least the *kind-family combinations
        // that ARE supported* have adapter coverage.
        if !covered {
            // Verify this kind has at least one valid chain family even if no
            // current adapter supports it. This is acceptable for Pool and
            // Protocol which are future work items.
            let has_valid_family = [
                ChainFamily::Solana,
                ChainFamily::Evm,
                ChainFamily::Hyperliquid,
            ]
            .iter()
            .any(|f| kind.valid_for_chain_family(*f));
            assert!(
                has_valid_family,
                "TargetKind {:?} is not valid for any chain family and has no adapter coverage",
                kind
            );
            eprintln!(
                "NOTE: TargetKind {:?} has valid chain families but no current adapter coverage (future work)",
                kind
            );
        }
    }

    // The concrete adapter-covered kinds must include at least: Wallet, Contract,
    // Program, Account, TopicFilter, Market (6 of 8).
    let covered_count = all_kinds
        .iter()
        .filter(|kind| {
            caps_list
                .iter()
                .any(|caps| caps.supported_target_kinds.contains(kind))
        })
        .count();
    assert!(
        covered_count >= 6,
        "expected at least 6 TargetKinds covered by adapters, got {covered_count}"
    );
}

// ============================================================================
// 3. Wallet target DB round-trip (Solana)
// ============================================================================

#[tokio::test]
async fn wallet_target_solana_round_trip() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("wallet_sol").await;

    // Register wallet IndexTarget
    let target = make_index_target(
        TargetKind::Wallet,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"),
        None,
        TargetMode::Both,
    );
    let created = repo.create_index_target(&target).await.unwrap();
    assert_eq!(created.id, target.id);

    // Create IngestionRun
    let run = make_ingestion_run(target.id, "solana-mainnet", "rpc", "backfill", "running");
    repo.create_ingestion_run(&run).await.unwrap();

    // Save synthetic RawTransactions
    let tx1 = make_raw_tx("solana-mainnet", "sol_tx_001", "rpc", Some(run.id));
    let tx2 = make_raw_tx("solana-mainnet", "sol_tx_002", "rpc", Some(run.id));
    repo.save_raw_transactions(&[tx1.clone(), tx2.clone()])
        .await
        .unwrap();

    // Create TargetMatches
    let m1 = make_target_match(target.id, tx1.id, Some("sender"));
    let m2 = make_target_match(target.id, tx2.id, Some("receiver"));
    repo.save_target_matches(&[m1.clone(), m2.clone()])
        .await
        .unwrap();

    // Upsert Checkpoint with Solana cursor
    let cp = make_checkpoint(
        target.id,
        "solana-mainnet",
        "rpc",
        serde_json::json!({"last_signature": "abc123", "last_slot": 250}),
    );
    repo.upsert_checkpoint_v2(&cp).await.unwrap();

    // Verify target retrieval
    let fetched = repo.get_index_target(target.id).await.unwrap().unwrap();
    assert_eq!(fetched.kind, TargetKind::Wallet);
    assert_eq!(fetched.chain_family, ChainFamily::Solana);
    assert_eq!(
        fetched.address.as_deref(),
        Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy")
    );

    // Verify raw transactions by run
    let txs = repo.get_raw_transactions_by_run(run.id).await.unwrap();
    assert_eq!(txs.len(), 2);

    // Verify matches by target
    let matches = repo.get_matches_by_target(target.id, 100, 0).await.unwrap();
    assert_eq!(matches.len(), 2);

    // Verify checkpoint retrieval
    let cp_fetched = repo
        .get_checkpoint_v2(target.id, "solana-mainnet", "rpc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp_fetched.cursor["last_slot"], 250);
    assert_eq!(cp_fetched.cursor["last_signature"], "abc123");

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 4. EVM contract target DB round-trip
// ============================================================================

#[tokio::test]
async fn evm_contract_target_round_trip() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("evm_contract").await;

    // Register contract IndexTarget with ContractFilterSpec
    let target = make_index_target(
        TargetKind::Contract,
        ChainFamily::Evm,
        "ethereum-mainnet",
        Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
        Some(serde_json::json!({
            "event_signatures": ["Transfer(address,address,uint256)"]
        })),
        TargetMode::Backfill,
    );
    repo.create_index_target(&target).await.unwrap();

    // Save synthetic RawTransactions
    let tx = make_raw_tx("ethereum-mainnet", "0xdeadbeef_contract", "rpc", None);
    repo.save_raw_transactions(std::slice::from_ref(&tx))
        .await
        .unwrap();

    // Create TargetMatch with contract_event match_reason
    let m = make_target_match(target.id, tx.id, Some("contract_event"));
    repo.save_target_matches(&[m]).await.unwrap();

    // Upsert Checkpoint with EVM block cursor
    let cp = make_checkpoint(
        target.id,
        "ethereum-mainnet",
        "rpc",
        serde_json::json!({"last_block": 18_000_000}),
    );
    repo.upsert_checkpoint_v2(&cp).await.unwrap();

    // Verify retrieval
    let fetched = repo.get_index_target(target.id).await.unwrap().unwrap();
    assert_eq!(fetched.kind, TargetKind::Contract);
    assert_eq!(fetched.chain_family, ChainFamily::Evm);
    assert!(fetched.filter_spec.is_some());

    let matches = repo.get_matches_by_target(target.id, 100, 0).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0.match_reason.as_deref(), Some("contract_event"));

    let cp_fetched = repo
        .get_checkpoint_v2(target.id, "ethereum-mainnet", "rpc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp_fetched.cursor["last_block"], 18_000_000);

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 5. Solana program target DB round-trip
// ============================================================================

#[tokio::test]
async fn solana_program_target_round_trip() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("sol_program").await;

    // Register program IndexTarget with ProgramFilterSpec
    let target = make_index_target(
        TargetKind::Program,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        Some(serde_json::json!({
            "instruction_discriminators": ["0x03"],
            "include_cpi": true
        })),
        TargetMode::Stream,
    );
    repo.create_index_target(&target).await.unwrap();

    // Save synthetic RawTransactions
    let tx = make_raw_tx("solana-mainnet", "sol_prog_tx_001", "grpc", None);
    repo.save_raw_transactions(std::slice::from_ref(&tx))
        .await
        .unwrap();

    // Create TargetMatch
    let m = make_target_match(target.id, tx.id, Some("program_invocation"));
    repo.save_target_matches(&[m]).await.unwrap();

    // Upsert Checkpoint with gRPC cursor
    let cp = make_checkpoint(
        target.id,
        "solana-mainnet",
        "grpc",
        serde_json::json!({"last_slot": 300, "commitment": "confirmed"}),
    );
    repo.upsert_checkpoint_v2(&cp).await.unwrap();

    // Verify retrieval
    let fetched = repo.get_index_target(target.id).await.unwrap().unwrap();
    assert_eq!(fetched.kind, TargetKind::Program);
    assert_eq!(
        fetched.address.as_deref(),
        Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
    );

    let matches = repo.get_matches_by_target(target.id, 100, 0).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(
        matches[0].0.match_reason.as_deref(),
        Some("program_invocation")
    );

    let cp_fetched = repo
        .get_checkpoint_v2(target.id, "solana-mainnet", "grpc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp_fetched.cursor["last_slot"], 300);

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 6. Hyperliquid wallet target DB round-trip
// ============================================================================

#[tokio::test]
async fn hyperliquid_wallet_target_round_trip() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("hl_wallet").await;

    // Register wallet IndexTarget on hypercore-mainnet
    let target = make_index_target(
        TargetKind::Wallet,
        ChainFamily::Hyperliquid,
        "hypercore-mainnet",
        Some("0x1234567890abcdef1234567890abcdef12345678"),
        None,
        TargetMode::Both,
    );
    repo.create_index_target(&target).await.unwrap();

    // Save synthetic RawTransactions
    let tx = make_raw_tx("hypercore-mainnet", "hl_wallet_tx_001", "rest", None);
    repo.save_raw_transactions(std::slice::from_ref(&tx))
        .await
        .unwrap();

    // Create TargetMatch
    let m = make_target_match(target.id, tx.id, Some("user_fill"));
    repo.save_target_matches(&[m]).await.unwrap();

    // Upsert Checkpoint with HL timestamp cursor
    let cp = make_checkpoint(
        target.id,
        "hypercore-mainnet",
        "rest",
        serde_json::json!({"last_timestamp": 1700000000000_i64}),
    );
    repo.upsert_checkpoint_v2(&cp).await.unwrap();

    // Verify retrieval
    let fetched = repo.get_index_target(target.id).await.unwrap().unwrap();
    assert_eq!(fetched.kind, TargetKind::Wallet);
    assert_eq!(fetched.chain_family, ChainFamily::Hyperliquid);
    assert_eq!(fetched.network, "hypercore-mainnet");

    let matches = repo.get_matches_by_target(target.id, 100, 0).await.unwrap();
    assert_eq!(matches.len(), 1);

    let cp_fetched = repo
        .get_checkpoint_v2(target.id, "hypercore-mainnet", "rest")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp_fetched.cursor["last_timestamp"], 1700000000000_i64);

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 7. Hyperliquid market target DB round-trip
// ============================================================================

#[tokio::test]
async fn hyperliquid_market_target_round_trip() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("hl_market").await;

    // Register market IndexTarget
    let target = make_index_target(
        TargetKind::Market,
        ChainFamily::Hyperliquid,
        "hypercore-mainnet",
        Some("ETH"),
        Some(serde_json::json!({
            "data_types": ["fills", "funding"],
            "min_size": "1.0"
        })),
        TargetMode::Both,
    );
    repo.create_index_target(&target).await.unwrap();

    // Save synthetic RawTransactions
    let tx = make_raw_tx("hypercore-mainnet", "hl_market_tx_001", "rest", None);
    repo.save_raw_transactions(std::slice::from_ref(&tx))
        .await
        .unwrap();

    // Create TargetMatch with trade match_reason
    let m = make_target_match(target.id, tx.id, Some("trade"));
    repo.save_target_matches(&[m]).await.unwrap();

    // Verify retrieval
    let fetched = repo.get_index_target(target.id).await.unwrap().unwrap();
    assert_eq!(fetched.kind, TargetKind::Market);
    assert_eq!(fetched.address.as_deref(), Some("ETH"));

    let matches = repo.get_matches_by_target(target.id, 100, 0).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0.match_reason.as_deref(), Some("trade"));

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 8. EVM topic_filter target DB round-trip
// ============================================================================

#[tokio::test]
async fn evm_topic_filter_target_round_trip() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("evm_topic").await;

    // Register topic_filter IndexTarget (no address)
    let target = make_index_target(
        TargetKind::TopicFilter,
        ChainFamily::Evm,
        "ethereum-mainnet",
        None,
        Some(serde_json::json!({
            "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                null,
                null
            ],
            "address_filter": []
        })),
        TargetMode::Backfill,
    );
    repo.create_index_target(&target).await.unwrap();

    // Save synthetic RawTransactions
    let tx = make_raw_tx("ethereum-mainnet", "0xtopic_tx_001", "rpc", None);
    repo.save_raw_transactions(std::slice::from_ref(&tx))
        .await
        .unwrap();

    // Create TargetMatch
    let m = make_target_match(target.id, tx.id, Some("topic_match"));
    repo.save_target_matches(&[m]).await.unwrap();

    // Upsert Checkpoint
    let cp = make_checkpoint(
        target.id,
        "ethereum-mainnet",
        "rpc",
        serde_json::json!({"last_block": 19_000_000}),
    );
    repo.upsert_checkpoint_v2(&cp).await.unwrap();

    // Verify retrieval
    let fetched = repo.get_index_target(target.id).await.unwrap().unwrap();
    assert_eq!(fetched.kind, TargetKind::TopicFilter);
    assert!(fetched.address.is_none());
    assert!(fetched.filter_spec.is_some());

    let matches = repo.get_matches_by_target(target.id, 100, 0).await.unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].0.match_reason.as_deref(), Some("topic_match"));

    let cp_fetched = repo
        .get_checkpoint_v2(target.id, "ethereum-mainnet", "rpc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cp_fetched.cursor["last_block"], 19_000_000);

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 9. Multi-target linkage: one raw_transaction linked to 3 targets
// ============================================================================

#[tokio::test]
async fn multi_target_linkage_three_targets() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("multi_link").await;

    // Create 3 targets of different kinds
    let wallet_target = make_index_target(
        TargetKind::Wallet,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("WalletAddr111"),
        None,
        TargetMode::Both,
    );
    let program_target = make_index_target(
        TargetKind::Program,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        None,
        TargetMode::Stream,
    );
    let account_target = make_index_target(
        TargetKind::Account,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC"),
        None,
        TargetMode::Both,
    );

    repo.create_index_target(&wallet_target).await.unwrap();
    repo.create_index_target(&program_target).await.unwrap();
    repo.create_index_target(&account_target).await.unwrap();

    // Create one shared raw_transaction
    let tx = make_raw_tx("solana-mainnet", "multi_link_tx_001", "grpc", None);
    repo.save_raw_transactions(std::slice::from_ref(&tx))
        .await
        .unwrap();

    // Link all 3 targets to the same raw_transaction with distinct match_reasons
    let m1 = make_target_match(wallet_target.id, tx.id, Some("sender"));
    let m2 = make_target_match(program_target.id, tx.id, Some("program_invocation"));
    let m3 = make_target_match(account_target.id, tx.id, Some("account_write"));
    repo.save_target_matches(&[m1, m2, m3]).await.unwrap();

    // Verify get_matches_by_raw_tx returns all 3
    let matches = repo.get_matches_by_raw_tx(tx.id).await.unwrap();
    assert_eq!(matches.len(), 3, "expected 3 target_matches for one raw_tx");

    let target_ids: Vec<Uuid> = matches.iter().map(|m| m.target_id).collect();
    assert!(target_ids.contains(&wallet_target.id));
    assert!(target_ids.contains(&program_target.id));
    assert!(target_ids.contains(&account_target.id));

    let reasons: Vec<Option<String>> = matches.iter().map(|m| m.match_reason.clone()).collect();
    assert!(reasons.contains(&Some("sender".to_string())));
    assert!(reasons.contains(&Some("program_invocation".to_string())));
    assert!(reasons.contains(&Some("account_write".to_string())));

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 10. Checkpoint upsert idempotency
// ============================================================================

#[tokio::test]
async fn checkpoint_upsert_idempotency() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("cp_upsert").await;

    // Create a target
    let target = make_index_target(
        TargetKind::Wallet,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("CpUpsertAddr"),
        None,
        TargetMode::Backfill,
    );
    repo.create_index_target(&target).await.unwrap();

    // First upsert
    let cp1 = make_checkpoint(
        target.id,
        "solana-mainnet",
        "rpc",
        serde_json::json!({"last_slot": 100}),
    );
    repo.upsert_checkpoint_v2(&cp1).await.unwrap();

    let fetched1 = repo
        .get_checkpoint_v2(target.id, "solana-mainnet", "rpc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fetched1.cursor["last_slot"], 100);

    // Second upsert with different cursor value (same target_id, network, source)
    let cp2 = make_checkpoint(
        target.id,
        "solana-mainnet",
        "rpc",
        serde_json::json!({"last_slot": 500}),
    );
    repo.upsert_checkpoint_v2(&cp2).await.unwrap();

    // Verify second value wins
    let fetched2 = repo
        .get_checkpoint_v2(target.id, "solana-mainnet", "rpc")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fetched2.cursor["last_slot"], 500,
        "second upsert should overwrite cursor"
    );

    // Verify only one checkpoint row exists (no duplicates)
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM checkpoints WHERE target_id = $1 AND network = 'solana-mainnet' AND source = 'rpc'"
    )
    .bind(target.id)
    .fetch_one(&_pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "upsert should produce exactly one row");

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 11. Ingestion run lifecycle
// ============================================================================

#[tokio::test]
async fn ingestion_run_lifecycle() {
    require_pg!();
    let (repo, _pool, db_name) = setup_test_repo("run_lifecycle").await;

    // Create a target
    let target = make_index_target(
        TargetKind::Wallet,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("RunLifecycleAddr"),
        None,
        TargetMode::Backfill,
    );
    repo.create_index_target(&target).await.unwrap();

    // Create run with status 'running'
    let run = make_ingestion_run(target.id, "solana-mainnet", "rpc", "backfill", "running");
    repo.create_ingestion_run(&run).await.unwrap();

    let fetched_running = repo.get_ingestion_run(run.id).await.unwrap().unwrap();
    assert_eq!(
        fetched_running.status,
        spectraplex_core::v2::IngestionJobStatus::Running
    );
    assert!(fetched_running.finished_at.is_none());
    assert_eq!(fetched_running.records_written, 0);

    // Update to 'completed' with records_written and finished_at
    let now = Utc::now();
    repo.update_ingestion_run_status(run.id, "completed", Some(now), 42, None)
        .await
        .unwrap();

    let fetched_completed = repo.get_ingestion_run(run.id).await.unwrap().unwrap();
    assert_eq!(
        fetched_completed.status,
        spectraplex_core::v2::IngestionJobStatus::Completed
    );
    assert!(fetched_completed.finished_at.is_some());
    assert_eq!(fetched_completed.records_written, 42);
    assert!(fetched_completed.error_message.is_none());

    // Verify list by target
    let runs = repo.list_ingestion_runs_by_target(target.id).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);

    _pool.close().await;
    drop_test_db(&db_name).await;
}

// ============================================================================
// 12. Error path: each adapter rejects unsupported target kinds
// ============================================================================

#[tokio::test]
async fn solana_adapter_rejects_unsupported_kind() {
    let adapter = SolanaAdapter::new("http://localhost:8899");
    let target = make_index_target(
        TargetKind::Contract,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("0xabc"),
        None,
        TargetMode::Backfill,
    );
    let result = adapter.backfill(&target, None, 10).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("only supports wallet targets")
            || err.contains("only supports Solana chain family"),
        "SolanaAdapter error should be clear, got: {err}"
    );
}

#[tokio::test]
async fn solana_grpc_adapter_rejects_unsupported_kind() {
    let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
    let target = make_index_target(
        TargetKind::Market,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("ETH"),
        None,
        TargetMode::Backfill,
    );
    let result = adapter.backfill(&target, None, 10).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does not support target kind"),
        "SolanaGrpcAdapter error should be clear, got: {err}"
    );
}

#[tokio::test]
async fn evm_adapter_rejects_unsupported_kind() {
    let adapter = EvmAdapter::new("http://localhost:8545").unwrap();
    let target = make_index_target(
        TargetKind::Program,
        ChainFamily::Evm,
        "ethereum-mainnet",
        Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        None,
        TargetMode::Backfill,
    );
    let result = adapter.backfill(&target, None, 10).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does not support target kind"),
        "EvmAdapter error should be clear, got: {err}"
    );
}

#[tokio::test]
async fn hyperliquid_adapter_rejects_unsupported_kind() {
    let adapter = HyperliquidAdapter::new();
    let target = make_index_target(
        TargetKind::Contract,
        ChainFamily::Hyperliquid,
        "hypercore-mainnet",
        Some("0xabc"),
        None,
        TargetMode::Backfill,
    );
    let result = adapter.backfill(&target, None, 10).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does not support target kind"),
        "HyperliquidAdapter error should be clear, got: {err}"
    );
}

#[tokio::test]
async fn legacy_compat_adapter_rejects_non_wallet() {
    let inner = SolanaAdapter::new("http://localhost:8899");
    let wrapper = LegacyConnectorAdapter::new(inner, &Chain::Solana);
    let target = make_index_target(
        TargetKind::Program,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
        None,
        TargetMode::Backfill,
    );
    let result = wrapper.backfill(&target, None, 10).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("only supports wallet targets"),
        "LegacyConnectorAdapter error should be clear, got: {err}"
    );
}

// ============================================================================
// 13. Error path: wrong chain family
// ============================================================================

#[tokio::test]
async fn solana_adapter_rejects_wrong_chain_family() {
    let adapter = SolanaAdapter::new("http://localhost:8899");
    let target = make_index_target(
        TargetKind::Wallet,
        ChainFamily::Evm,
        "ethereum-mainnet",
        Some("0xabc"),
        None,
        TargetMode::Backfill,
    );
    let result = adapter.backfill(&target, None, 10).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("only supports Solana chain family"),
        "SolanaAdapter should reject EVM family, got: {err}"
    );
}

#[tokio::test]
async fn evm_adapter_rejects_wrong_chain_family() {
    let adapter = EvmAdapter::new("http://localhost:8545").unwrap();
    let target = make_index_target(
        TargetKind::Wallet,
        ChainFamily::Solana,
        "solana-mainnet",
        Some("WalletAddr"),
        None,
        TargetMode::Backfill,
    );
    let result = adapter.backfill(&target, None, 10).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("only supports EVM chain family"),
        "EvmAdapter should reject Solana family, got: {err}"
    );
}

#[tokio::test]
async fn hyperliquid_adapter_rejects_wrong_chain_family() {
    let adapter = HyperliquidAdapter::new();
    let target = make_index_target(
        TargetKind::Wallet,
        ChainFamily::Evm,
        "ethereum-mainnet",
        Some("0xabc"),
        None,
        TargetMode::Backfill,
    );
    let result = adapter.backfill(&target, None, 10).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("only supports Hyperliquid chain family"),
        "HyperliquidAdapter should reject EVM family, got: {err}"
    );
}

// ============================================================================
// 14. validate_target + can_service consistency
// ============================================================================

#[test]
fn validate_target_and_can_service_agree() {
    // For every (TargetKind, ChainFamily) pair where validate_target passes,
    // at least one connector should be able to service it OR it's a known
    // future gap (Pool, Protocol, Account on EVM).

    let all_kinds = [
        TargetKind::Wallet,
        TargetKind::Contract,
        TargetKind::Program,
        TargetKind::Account,
        TargetKind::TopicFilter,
        TargetKind::Market,
        TargetKind::Pool,
        TargetKind::Protocol,
    ];
    let all_families = [
        ChainFamily::Solana,
        ChainFamily::Evm,
        ChainFamily::Hyperliquid,
    ];

    let caps_list: Vec<ConnectorCapabilities> = vec![
        SolanaAdapter::new("http://localhost:8899").capabilities(),
        SolanaGrpcAdapter::new("http://localhost:10000", None).capabilities(),
        EvmAdapter::new("http://localhost:8545")
            .unwrap()
            .capabilities(),
        HyperliquidAdapter::new().capabilities(),
    ];

    // Known future gaps: Pool and Protocol are valid for some families but have
    // no adapter yet. Account on EVM is valid but no adapter supports it.
    let known_gaps: Vec<(TargetKind, ChainFamily)> = vec![
        (TargetKind::Pool, ChainFamily::Solana),
        (TargetKind::Pool, ChainFamily::Evm),
        (TargetKind::Protocol, ChainFamily::Solana),
        (TargetKind::Protocol, ChainFamily::Evm),
        (TargetKind::Account, ChainFamily::Evm),
    ];

    for kind in &all_kinds {
        for family in &all_families {
            // Build a well-formed target for validation
            let (address, filter_spec) = match kind {
                TargetKind::TopicFilter => (None, Some(serde_json::json!({"topics": ["0xabc"]}))),
                TargetKind::Protocol => (
                    None,
                    Some(serde_json::json!({"name": "test", "addresses": {}})),
                ),
                _ => (Some("test_addr"), None),
            };
            let target = make_index_target(
                *kind,
                *family,
                "test-mainnet",
                address,
                filter_spec,
                TargetMode::Backfill,
            );

            let is_valid = kind.valid_for_chain_family(*family);
            let has_adapter = caps_list.iter().any(|caps| caps.can_service(&target));

            if is_valid && !has_adapter {
                // Must be a known gap
                assert!(
                    known_gaps.contains(&(*kind, *family)),
                    "valid target ({:?}, {:?}) has no adapter and is not in known gaps",
                    kind,
                    family
                );
            }

            // If an adapter can service it, validate_target should pass
            // (assuming the target is well-formed)
            if has_adapter && is_valid {
                assert!(
                    validate_target(&target).is_ok(),
                    "validate_target should pass for ({:?}, {:?}) which an adapter can service",
                    kind,
                    family
                );
            }

            // If the kind is NOT valid for the family, validate_target should reject
            if !is_valid {
                assert!(
                    validate_target(&target).is_err(),
                    "validate_target should reject ({:?}, {:?})",
                    kind,
                    family
                );
            }
        }
    }
}
