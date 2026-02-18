use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use spectraplex_adapters::{
    evm::EvmAdapter,
    evm_parser,
    hyperliquid::HyperliquidAdapter,
    hyperliquid_parser,
    repo::{build_checkpoint, Repository},
    solana::SolanaAdapter,
    solana_parser,
};
use spectraplex_core::config::AppConfig;
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, LedgerEntry, Transaction};
use sqlx::postgres::PgPoolOptions;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn internal(e: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: e.to_string(),
        }
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!(status = %self.status, error = %self.message);
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

struct AppState {
    repo: Repository,
    config: AppConfig,
    jobs: RwLock<HashMap<Uuid, JobEntry>>,
}

/// Wraps a JobStatus with a timestamp for TTL-based cleanup.
struct JobEntry {
    status: JobStatus,
    finished_at: Option<Instant>,
}

const JOB_TTL_SECS: u64 = 3600; // 1 hour

impl AppState {
    /// Remove completed/failed jobs older than JOB_TTL_SECS.
    async fn prune_stale_jobs(&self) {
        let mut jobs = self.jobs.write().await;
        let cutoff = Instant::now() - std::time::Duration::from_secs(JOB_TTL_SECS);
        jobs.retain(|_, entry| entry.finished_at.is_none_or(|finished| finished > cutoff));
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub id: Uuid,
    pub state: JobState,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Running,
    Completed,
    Failed,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let config = AppConfig::load()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| config.log_level.clone().into()),
        )
        .init();

    let pool = PgPoolOptions::new()
        .max_connections(config.pool_size)
        .connect(&config.database_url)
        .await?;

    let shared_state = Arc::new(AppState {
        repo: Repository::new(pool),
        config: config.clone(),
        jobs: RwLock::new(HashMap::new()),
    });

    let protected = Router::new()
        .route("/v1/ingest", post(trigger_ingest))
        .route("/v1/normalize", post(trigger_normalize))
        .route("/v1/jobs/{job_id}", get(get_job_status))
        .route("/v1/transactions/{wallet}", get(get_transactions))
        .route("/v1/ledger/{wallet}", get(get_ledger))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&shared_state),
            require_auth,
        ));

    let app = Router::new()
        .route("/health", get(health_check))
        .merge(protected)
        .layer(TraceLayer::new_for_http())
        .with_state(shared_state);

    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    info!("Listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check() -> &'static str {
    "OK"
}

async fn require_auth(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let expected = match &state.config.api_key {
        Some(key) => key,
        None => return Ok(next.run(req).await),
    };

    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match header {
        Some(token) if token == expected => Ok(next.run(req).await),
        _ => Err(AppError {
            status: StatusCode::UNAUTHORIZED,
            message: "Missing or invalid API key".to_string(),
        }),
    }
}

#[derive(Deserialize)]
struct IngestRequest {
    chain: String,
    wallet: String,
    user_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct NormalizeRequest {
    wallet: String,
}

const DEFAULT_PAGE_LIMIT: i64 = 50;
const MAX_PAGE_LIMIT: i64 = 1000;

#[derive(Deserialize)]
struct PaginationParams {
    limit: Option<i64>,
    offset: Option<i64>,
}

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

fn clamp_offset(offset: Option<i64>) -> i64 {
    offset.unwrap_or(0).max(0)
}

/// Basic wallet address validation. Rejects obviously invalid inputs.
fn validate_wallet(wallet: &str) -> Result<(), AppError> {
    if wallet.is_empty() || wallet.len() > 128 {
        return Err(AppError::bad_request("Invalid wallet address length"));
    }
    if !wallet
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == 'x')
    {
        return Err(AppError::bad_request(
            "Wallet address contains invalid characters",
        ));
    }
    Ok(())
}

async fn trigger_ingest(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<IngestRequest>,
) -> Result<Json<JobStatus>, AppError> {
    validate_wallet(&payload.wallet)?;
    let job_id = Uuid::new_v4();
    let job = JobStatus {
        id: job_id,
        state: JobState::Pending,
        message: None,
    };

    state.jobs.write().await.insert(
        job_id,
        JobEntry {
            status: job.clone(),
            finished_at: None,
        },
    );

    let state_clone = Arc::clone(&state);
    let wallet = payload.wallet.clone();
    let chain = payload.chain.clone();
    let limit = state.config.ingest_limit;
    let user_id = payload.user_id.unwrap_or_else(Uuid::new_v4);

    tokio::spawn(async move {
        {
            let mut jobs = state_clone.jobs.write().await;
            if let Some(entry) = jobs.get_mut(&job_id) {
                entry.status.state = JobState::Running;
            }
        }

        let result = async {
            let checkpoint: Option<IndexerCheckpoint> = state_clone
                .repo
                .get_checkpoint(&chain, &wallet)
                .await
                .unwrap_or(None);

            let events: Vec<Transaction> = match chain.as_str() {
                "hyperliquid" => {
                    let adapter = HyperliquidAdapter::new();
                    adapter
                        .fetch_history(&wallet, limit, user_id, checkpoint.as_ref())
                        .await?
                }
                "ethereum" => {
                    let adapter = EvmAdapter::new(&state_clone.config.evm_rpc_url).await?;
                    adapter
                        .fetch_history(&wallet, limit, user_id, checkpoint.as_ref())
                        .await?
                }
                _ => {
                    let adapter = SolanaAdapter::new(&state_clone.config.solana_rpc_url);
                    adapter
                        .fetch_history(&wallet, limit, user_id, checkpoint.as_ref())
                        .await?
                }
            };
            let count = events.len();
            if let Some(cp) = build_checkpoint(&chain, &wallet, &events) {
                state_clone
                    .repo
                    .save_transactions_and_checkpoint(&events, &cp)
                    .await?;
            } else {
                state_clone.repo.save_transactions(&events).await?;
            }
            Ok::<usize, anyhow::Error>(count)
        }
        .await;

        let mut jobs = state_clone.jobs.write().await;
        if let Some(entry) = jobs.get_mut(&job_id) {
            match result {
                Ok(count) => {
                    info!(job_id = %job_id, count, "Ingestion completed");
                    entry.status.state = JobState::Completed;
                    entry.status.message = Some(format!("Ingested {} transactions", count));
                }
                Err(e) => {
                    error!(job_id = %job_id, error = %e, "Ingestion failed");
                    entry.status.state = JobState::Failed;
                    entry.status.message = Some(e.to_string());
                }
            }
            entry.finished_at = Some(Instant::now());
        }

        state_clone.prune_stale_jobs().await;
    });

    info!(job_id = %job_id, "Ingestion job queued");
    Ok(Json(JobStatus {
        id: job_id,
        state: JobState::Pending,
        message: Some("Job queued".to_string()),
    }))
}

async fn trigger_normalize(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<NormalizeRequest>,
) -> Result<Json<JobStatus>, AppError> {
    validate_wallet(&payload.wallet)?;
    let job_id = Uuid::new_v4();
    let job = JobStatus {
        id: job_id,
        state: JobState::Pending,
        message: None,
    };

    state.jobs.write().await.insert(
        job_id,
        JobEntry {
            status: job.clone(),
            finished_at: None,
        },
    );

    let state_clone = Arc::clone(&state);
    let wallet = payload.wallet.clone();

    tokio::spawn(async move {
        {
            let mut jobs = state_clone.jobs.write().await;
            if let Some(entry) = jobs.get_mut(&job_id) {
                entry.status.state = JobState::Running;
            }
        }

        let result = async {
            let txs = state_clone.repo.get_transactions_by_wallet(&wallet).await?;

            let mut all_entries = Vec::new();
            for tx in txs {
                let result = match tx.chain {
                    spectraplex_core::models::Chain::Solana => {
                        solana_parser::parse_solana_transaction(&tx)
                    }
                    spectraplex_core::models::Chain::Hyperliquid => {
                        hyperliquid_parser::parse_hyperliquid_transaction(&tx)
                    }
                    spectraplex_core::models::Chain::Ethereum => {
                        evm_parser::parse_evm_transaction(&tx)
                    }
                };
                match result {
                    Ok(entries) => all_entries.extend(entries),
                    Err(e) => {
                        error!(tx_hash = %tx.tx_hash, error = %e, "Skipping unparseable transaction");
                    }
                }
            }

            let count = all_entries.len();
            state_clone.repo.save_ledger_entries(&all_entries).await?;
            Ok::<usize, anyhow::Error>(count)
        }
        .await;

        let mut jobs = state_clone.jobs.write().await;
        if let Some(entry) = jobs.get_mut(&job_id) {
            match result {
                Ok(count) => {
                    info!(job_id = %job_id, count, "Normalization completed");
                    entry.status.state = JobState::Completed;
                    entry.status.message = Some(format!("Normalized {} ledger entries", count));
                }
                Err(e) => {
                    error!(job_id = %job_id, error = %e, "Normalization failed");
                    entry.status.state = JobState::Failed;
                    entry.status.message = Some(e.to_string());
                }
            }
            entry.finished_at = Some(Instant::now());
        }

        state_clone.prune_stale_jobs().await;
    });

    info!(job_id = %job_id, "Normalization job queued");
    Ok(Json(JobStatus {
        id: job_id,
        state: JobState::Pending,
        message: Some("Job queued".to_string()),
    }))
}

async fn get_job_status(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<Uuid>,
) -> Result<Json<JobStatus>, AppError> {
    let jobs = state.jobs.read().await;
    match jobs.get(&job_id) {
        Some(entry) => Ok(Json(entry.status.clone())),
        None => Err(AppError::not_found(format!("Job {} not found", job_id))),
    }
}

async fn get_transactions(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Vec<Transaction>>, AppError> {
    validate_wallet(&wallet)?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let txs = state
        .repo
        .get_transactions_by_wallet_paginated(&wallet, limit, offset)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(txs))
}

async fn get_ledger(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Vec<LedgerEntry>>, AppError> {
    validate_wallet(&wallet)?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let entries = state
        .repo
        .get_ledger_entries_by_wallet_paginated(&wallet, limit, offset)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(entries))
}

fn build_checkpoint(chain: &str, wallet: &str, txs: &[Transaction]) -> Option<IndexerCheckpoint> {
    if txs.is_empty() {
        return None;
    }

    let chain_enum = match chain {
        "solana" => Chain::Solana,
        "ethereum" => Chain::Ethereum,
        "hyperliquid" => Chain::Hyperliquid,
        _ => return None,
    };

    let latest = txs.iter().max_by_key(|tx| tx.timestamp)?;

    let last_signature = Some(latest.tx_hash.clone());
    let last_timestamp = Some(latest.timestamp);

    let last_slot = match chain {
        "solana" => txs
            .iter()
            .filter_map(|tx| tx.raw_metadata.get("slot").and_then(|v| v.as_i64()))
            .max(),
        _ => None,
    };

    let last_block = match chain {
        "ethereum" => txs
            .iter()
            .filter_map(|tx| tx.raw_metadata.get("block_number").and_then(|v| v.as_i64()))
            .max(),
        _ => None,
    };

    Some(IndexerCheckpoint {
        chain: chain_enum,
        wallet_address: wallet.to_string(),
        last_signature,
        last_slot,
        last_block,
        last_timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        test_state_with_key(None)
    }

    fn test_state_with_key(api_key: Option<String>) -> Arc<AppState> {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fake:fake@localhost/fake")
            .unwrap();
        let config = AppConfig {
            api_key,
            ..AppConfig::default()
        };
        Arc::new(AppState {
            repo: Repository::new(pool),
            config,
            jobs: RwLock::new(HashMap::new()),
        })
    }

    fn test_router() -> Router {
        let state = test_state();
        test_router_with_state(state)
    }

    fn test_router_with_state(state: Arc<AppState>) -> Router {
        let protected = Router::new()
            .route("/v1/ingest", post(trigger_ingest))
            .route("/v1/normalize", post(trigger_normalize))
            .route("/v1/jobs/{job_id}", get(get_job_status))
            .route("/v1/transactions/{wallet}", get(get_transactions))
            .route("/v1/ledger/{wallet}", get(get_ledger))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                require_auth,
            ));

        Router::new()
            .route("/health", get(health_check))
            .merge(protected)
            .with_state(state)
    }

    #[test]
    fn test_validate_wallet_valid() {
        assert!(validate_wallet("abc123").is_ok());
        assert!(validate_wallet("0xabcdef1234567890").is_ok());
        assert!(validate_wallet("SoLaNaWaLLeT123").is_ok());
    }

    #[test]
    fn test_validate_wallet_empty() {
        let err = validate_wallet("").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_wallet_too_long() {
        let long = "a".repeat(129);
        let err = validate_wallet(&long).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_wallet_max_length() {
        let max = "a".repeat(128);
        assert!(validate_wallet(&max).is_ok());
    }

    #[test]
    fn test_validate_wallet_invalid_chars() {
        let err = validate_wallet("wallet-with-dashes").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        let err = validate_wallet("wallet with spaces").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        let err = validate_wallet("wallet;drop table").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_clamp_limit_default() {
        assert_eq!(clamp_limit(None), DEFAULT_PAGE_LIMIT);
    }

    #[test]
    fn test_clamp_limit_within_range() {
        assert_eq!(clamp_limit(Some(100)), 100);
    }

    #[test]
    fn test_clamp_limit_too_high() {
        assert_eq!(clamp_limit(Some(5000)), MAX_PAGE_LIMIT);
    }

    #[test]
    fn test_clamp_limit_too_low() {
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(-10)), 1);
    }

    #[test]
    fn test_clamp_offset_default() {
        assert_eq!(clamp_offset(None), 0);
    }

    #[test]
    fn test_clamp_offset_positive() {
        assert_eq!(clamp_offset(Some(50)), 50);
    }

    #[test]
    fn test_clamp_offset_negative() {
        assert_eq!(clamp_offset(Some(-5)), 0);
    }

    #[test]
    fn test_app_error_into_response() {
        let err = AppError::bad_request("test error");
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_app_error_not_found() {
        let err = AppError::not_found("missing");
        assert_eq!(err.status, StatusCode::NOT_FOUND);
        assert_eq!(err.message, "missing");
    }

    #[test]
    fn test_app_error_internal() {
        let err = AppError::internal("something broke");
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.message, "something broke");
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"OK");
    }

    #[tokio::test]
    async fn test_job_status_not_found() {
        let app = test_router();
        let job_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .uri(format!("/v1/jobs/{}", job_id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_job_status_found() {
        let state = test_state();
        let job_id = Uuid::new_v4();
        state.jobs.write().await.insert(
            job_id,
            JobEntry {
                status: JobStatus {
                    id: job_id,
                    state: JobState::Completed,
                    message: Some("done".to_string()),
                },
                finished_at: Some(Instant::now()),
            },
        );

        let app = test_router_with_state(Arc::clone(&state));

        let req = axum::http::Request::builder()
            .uri(format!("/v1/jobs/{}", job_id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(job["state"], "completed");
        assert_eq!(job["message"], "done");
    }

    #[tokio::test]
    async fn test_ingest_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "bad;wallet"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_normalize_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/normalize")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallet": "bad;wallet"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_transactions_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/bad%20wallet")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_ledger_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/ledger/bad%20wallet")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_nonexistent_route_returns_404() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/nonexistent")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_ingest_missing_body() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert!(response.status().is_client_error());
    }

    #[tokio::test]
    async fn test_prune_stale_jobs() {
        let state = test_state();
        let old_id = Uuid::new_v4();
        let new_id = Uuid::new_v4();

        state.jobs.write().await.insert(
            old_id,
            JobEntry {
                status: JobStatus {
                    id: old_id,
                    state: JobState::Completed,
                    message: None,
                },
                finished_at: Some(
                    Instant::now() - std::time::Duration::from_secs(JOB_TTL_SECS + 1),
                ),
            },
        );
        state.jobs.write().await.insert(
            new_id,
            JobEntry {
                status: JobStatus {
                    id: new_id,
                    state: JobState::Running,
                    message: None,
                },
                finished_at: None,
            },
        );

        state.prune_stale_jobs().await;

        let jobs = state.jobs.read().await;
        assert!(!jobs.contains_key(&old_id));
        assert!(jobs.contains_key(&new_id));
    }

    #[test]
    fn test_job_state_serialization() {
        let status = JobStatus {
            id: Uuid::nil(),
            state: JobState::Pending,
            message: Some("test".to_string()),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "pending");
        assert_eq!(json["message"], "test");
    }

    #[test]
    fn test_job_state_variants_serialize() {
        for (variant, expected) in [
            (JobState::Pending, "pending"),
            (JobState::Running, "running"),
            (JobState::Completed, "completed"),
            (JobState::Failed, "failed"),
        ] {
            let status = JobStatus {
                id: Uuid::nil(),
                state: variant,
                message: None,
            };
            let json = serde_json::to_value(&status).unwrap();
            assert_eq!(json["state"], expected);
        }
    }

    #[tokio::test]
    async fn test_auth_health_no_key_required() {
        let state = test_state_with_key(Some("secret".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_auth_rejects_missing_header() {
        let state = test_state_with_key(Some("secret".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_rejects_wrong_key() {
        let state = test_state_with_key(Some("secret".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", "Bearer wrong-key")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_accepts_valid_key() {
        let state = test_state_with_key(Some("secret".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_skipped_when_no_key_configured() {
        let state = test_state_with_key(None);
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_rejects_non_bearer_scheme() {
        let state = test_state_with_key(Some("secret".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", "Basic secret")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_auth_protects_get_endpoints() {
        let state = test_state_with_key(Some("secret".to_string()));
        let app = test_router_with_state(state);
        let job_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .uri(format!("/v1/jobs/{}", job_id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    fn make_tx(
        chain: Chain,
        tx_hash: &str,
        timestamp: i64,
        metadata: serde_json::Value,
    ) -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::nil(),
            wallet_address: "test_wallet".to_string(),
            timestamp,
            tx_hash: tx_hash.to_string(),
            chain,
            raw_metadata: metadata,
        }
    }

    #[test]
    fn test_build_checkpoint_empty() {
        assert!(build_checkpoint("solana", "wallet", &[]).is_none());
    }

    #[test]
    fn test_build_checkpoint_unknown_chain() {
        let tx = make_tx(Chain::Ethereum, "0xaaa", 100, serde_json::json!({}));
        assert!(build_checkpoint("bitcoin", "wallet", &[tx]).is_none());
    }

    #[test]
    fn test_build_checkpoint_solana() {
        let txs = vec![
            make_tx(
                Chain::Solana,
                "sig1",
                100,
                serde_json::json!({"slot": 5000}),
            ),
            make_tx(
                Chain::Solana,
                "sig2",
                200,
                serde_json::json!({"slot": 6000}),
            ),
        ];
        let cp = build_checkpoint("solana", "wallet", &txs).unwrap();
        assert!(matches!(cp.chain, Chain::Solana));
        assert_eq!(cp.last_signature, Some("sig2".to_string()));
        assert_eq!(cp.last_timestamp, Some(200));
        assert_eq!(cp.last_slot, Some(6000));
        assert_eq!(cp.last_block, None);
    }

    #[test]
    fn test_build_checkpoint_ethereum() {
        let txs = vec![make_tx(
            Chain::Ethereum,
            "0xaaa",
            100,
            serde_json::json!({"block_number": 1000}),
        )];
        let cp = build_checkpoint("ethereum", "0xwallet", &txs).unwrap();
        assert!(matches!(cp.chain, Chain::Ethereum));
        assert_eq!(cp.last_block, Some(1000));
        assert_eq!(cp.last_slot, None);
    }

    #[test]
    fn test_build_checkpoint_hyperliquid() {
        let txs = vec![make_tx(
            Chain::Hyperliquid,
            "hash1",
            500,
            serde_json::json!({}),
        )];
        let cp = build_checkpoint("hyperliquid", "0xhl", &txs).unwrap();
        assert!(matches!(cp.chain, Chain::Hyperliquid));
        assert_eq!(cp.last_signature, Some("hash1".to_string()));
        assert_eq!(cp.last_timestamp, Some(500));
    }
}
