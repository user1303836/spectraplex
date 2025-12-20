use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use spectraplex_adapters::{repo::Repository, solana::SolanaAdapter, solana_parser};
use spectraplex_core::models::{ChainIngestor, LedgerEntry, Transaction};
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::net::SocketAddr;
use std::sync::Arc;

// App State to share DB Pool
struct AppState {
    pool: PgPool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;

    let shared_state = Arc::new(AppState { pool });

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/v1/ingest", post(trigger_ingest))
        .route("/v1/normalize", post(trigger_normalize))
        .route("/v1/transactions/:wallet", get(get_transactions))
        .route("/v1/ledger/:wallet", get(get_ledger))
        .with_state(shared_state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}

async fn health_check() -> &'static str {
    "OK"
}

// Request Models
#[derive(Deserialize)]
struct IngestRequest {
    _chain: String,
    wallet: String,
    rpc_url: String,
}

#[derive(Deserialize)]
struct NormalizeRequest {
    wallet: String,
}

// Handlers

async fn trigger_ingest(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<IngestRequest>,
) -> Result<Json<String>, StatusCode> {
    // In a real system, this should spawn a background task or push to a queue (e.g. Redis/bullmq)
    // For prototype, we'll just run it inline (blocking the request until done - not ideal for prod but ok for demo)
    
    let adapter = SolanaAdapter::new(&payload.rpc_url);
    // Hardcoded limit for API safety
    let events = adapter.fetch_history(&payload.wallet, 50).await.map_err(|e| {
        eprintln!("Ingest Error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let repo = Repository::new(state.pool.clone());
    repo.save_transactions(&events).await.map_err(|e| {
        eprintln!("DB Error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(format!("Ingested {} transactions", events.len())))
}

async fn trigger_normalize(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<NormalizeRequest>,
) -> Result<Json<String>, StatusCode> {
    let repo = Repository::new(state.pool.clone());
    
    let txs = repo.get_transactions_by_wallet(&payload.wallet).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    let mut all_entries = Vec::new();

    for tx in txs {
        let entries = match tx.chain {
            spectraplex_core::models::Chain::Solana => {
                solana_parser::parse_solana_transaction(&tx).unwrap_or_default()
            },
            _ => vec![]
        };
        all_entries.extend(entries);
    }

    repo.save_ledger_entries(&all_entries).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(format!("Normalized {} ledger entries", all_entries.len())))
}

async fn get_transactions(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
) -> Result<Json<Vec<Transaction>>, StatusCode> {
    let repo = Repository::new(state.pool.clone());
    let txs = repo.get_transactions_by_wallet(&wallet).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(txs))
}

async fn get_ledger(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
) -> Result<Json<Vec<LedgerEntry>>, StatusCode> {
    let repo = Repository::new(state.pool.clone());
    let entries = repo.get_ledger_entries_by_wallet(&wallet).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(entries)) 
}
