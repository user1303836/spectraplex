use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use spectraplex_adapters::{
    hyperliquid::HyperliquidAdapter, hyperliquid_parser, repo::Repository, solana::SolanaAdapter,
    solana_parser,
};
use spectraplex_core::config::AppConfig;
use spectraplex_core::models::{ChainIngestor, LedgerEntry, Transaction};
use sqlx::postgres::PgPoolOptions;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

struct AppState {
    pool: sqlx::PgPool,
    config: AppConfig,
    jobs: RwLock<HashMap<Uuid, JobStatus>>,
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

    let config = AppConfig::load().expect("Failed to load config");

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

    let addr = SocketAddr::from(([127, 0, 0, 1], config.port));
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
    _chain: String,
    wallet: String,
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
) -> Result<Json<JobStatus>, StatusCode> {
    let job_id = Uuid::new_v4();
    let job = JobStatus {
        id: job_id,
        state: JobState::Pending,
        message: None,
    };

    state.jobs.write().await.insert(job_id, job.clone());

    let state_clone = Arc::clone(&state);
    let wallet = payload.wallet.clone();
    let chain = payload._chain.clone();
    let limit = state.config.ingest_limit;

    tokio::spawn(async move {
        {
            let mut jobs = state_clone.jobs.write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                j.state = JobState::Running;
            }
        }

        let result = async {
            let events: Vec<Transaction> = match chain.as_str() {
                "hyperliquid" => {
                    let adapter = HyperliquidAdapter::new();
                    adapter.fetch_history(&wallet, limit).await?
                }
                _ => {
                    let adapter = SolanaAdapter::new(&state_clone.config.solana_rpc_url);
                    adapter.fetch_history(&wallet, limit).await?
                }
            };
            let repo = Repository::new(state_clone.pool.clone());
            repo.save_transactions(&events).await?;
            Ok::<usize, anyhow::Error>(events.len())
        }
        .await;

        let mut jobs = state_clone.jobs.write().await;
        if let Some(j) = jobs.get_mut(&job_id) {
            match result {
                Ok(count) => {
                    info!(job_id = %job_id, count, "Ingestion completed");
                    j.state = JobState::Completed;
                    j.message = Some(format!("Ingested {} transactions", count));
                }
                Err(e) => {
                    error!(job_id = %job_id, error = %e, "Ingestion failed");
                    j.state = JobState::Failed;
                    j.message = Some(e.to_string());
                }
            }
        }
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
) -> Result<Json<JobStatus>, StatusCode> {
    let job_id = Uuid::new_v4();
    let job = JobStatus {
        id: job_id,
        state: JobState::Pending,
        message: None,
    };

    state.jobs.write().await.insert(job_id, job.clone());

    let state_clone = Arc::clone(&state);
    let wallet = payload.wallet.clone();

    tokio::spawn(async move {
        {
            let mut jobs = state_clone.jobs.write().await;
            if let Some(j) = jobs.get_mut(&job_id) {
                j.state = JobState::Running;
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
                    _ => vec![],
                };
                all_entries.extend(entries);
            }

            let count = all_entries.len();
            repo.save_ledger_entries(&all_entries).await?;
            Ok::<usize, anyhow::Error>(count)
        }
        .await;

        let mut jobs = state_clone.jobs.write().await;
        if let Some(j) = jobs.get_mut(&job_id) {
            match result {
                Ok(count) => {
                    info!(job_id = %job_id, count, "Normalization completed");
                    j.state = JobState::Completed;
                    j.message = Some(format!("Normalized {} ledger entries", count));
                }
                Err(e) => {
                    error!(job_id = %job_id, error = %e, "Normalization failed");
                    j.state = JobState::Failed;
                    j.message = Some(e.to_string());
                }
            }
        }
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
) -> Result<Json<JobStatus>, StatusCode> {
    let jobs = state.jobs.read().await;
    match jobs.get(&job_id) {
        Some(status) => Ok(Json(status.clone())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn get_transactions(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Vec<Transaction>>, StatusCode> {
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let repo = Repository::new(state.pool.clone());
    let txs = repo
        .get_transactions_by_wallet_paginated(&wallet, limit, offset)
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to fetch transactions");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(txs))
}

async fn get_ledger(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
    Query(params): Query<PaginationParams>,
) -> Result<Json<Vec<LedgerEntry>>, StatusCode> {
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let repo = Repository::new(state.pool.clone());
    let entries = repo
        .get_ledger_entries_by_wallet_paginated(&wallet, limit, offset)
        .await
        .map_err(|e| {
            error!(error = %e, "Failed to fetch ledger entries");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(entries))
}
