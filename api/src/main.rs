use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use spectraplex_adapters::{
    evm::EvmAdapter, evm_parser, hyperliquid::HyperliquidAdapter, hyperliquid_parser,
    repo::Repository, solana::SolanaAdapter, solana_parser,
};
use spectraplex_core::config::AppConfig;
use spectraplex_core::models::{ChainIngestor, LedgerEntry, Transaction};
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
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!(status = %self.status, error = %self.message);
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

struct AppState {
    pool: sqlx::PgPool,
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
    dotenv::dotenv().ok();

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
        pool,
        config: config.clone(),
        jobs: RwLock::new(HashMap::new()),
    });

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/v1/ingest", post(trigger_ingest))
        .route("/v1/normalize", post(trigger_normalize))
        .route("/v1/jobs/{job_id}", get(get_job_status))
        .route("/v1/transactions/{wallet}", get(get_transactions))
        .route("/v1/ledger/{wallet}", get(get_ledger))
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

async fn trigger_ingest(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<IngestRequest>,
) -> Result<Json<JobStatus>, AppError> {
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
            let events: Vec<Transaction> = match chain.as_str() {
                "hyperliquid" => {
                    let adapter = HyperliquidAdapter::new();
                    adapter.fetch_history(&wallet, limit, user_id).await?
                }
                "ethereum" => {
                    let adapter = EvmAdapter::new(&state_clone.config.evm_rpc_url).await?;
                    adapter.fetch_history(&wallet, limit, user_id).await?
                }
                _ => {
                    let adapter = SolanaAdapter::new(&state_clone.config.solana_rpc_url);
                    adapter.fetch_history(&wallet, limit, user_id).await?
                }
            };
            let repo = Repository::new(state_clone.pool.clone());
            repo.save_transactions(&events).await?;
            Ok::<usize, anyhow::Error>(events.len())
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
            let repo = Repository::new(state_clone.pool.clone());
            let txs = repo.get_transactions_by_wallet(&wallet).await?;

            let mut all_entries = Vec::new();
            for tx in txs {
                let entries = match tx.chain {
                    spectraplex_core::models::Chain::Solana => {
                        solana_parser::parse_solana_transaction(&tx).unwrap_or_default()
                    }
                    spectraplex_core::models::Chain::Hyperliquid => {
                        hyperliquid_parser::parse_hyperliquid_transaction(&tx).unwrap_or_default()
                    }
                    spectraplex_core::models::Chain::Ethereum => {
                        evm_parser::parse_evm_transaction(&tx).unwrap_or_default()
                    }
                };
                all_entries.extend(entries);
            }

            let count = all_entries.len();
            repo.save_ledger_entries(&all_entries).await?;
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
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let repo = Repository::new(state.pool.clone());
    let txs = repo
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
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let repo = Repository::new(state.pool.clone());
    let entries = repo
        .get_ledger_entries_by_wallet_paginated(&wallet, limit, offset)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(entries))
}
