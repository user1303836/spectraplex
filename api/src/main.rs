use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};
use spectraplex_adapters::{
    evm::EvmAdapter,
    evm_parser,
    hyperliquid::HyperliquidAdapter,
    hyperliquid_parser,
    hyperliquid_ws::HyperliquidWsClient,
    repo::{build_checkpoint, Repository},
    solana::SolanaAdapter,
    solana_grpc::SolanaGrpcAdapter,
    solana_parser,
};
use spectraplex_core::config::AppConfig;
use spectraplex_core::models::{ChainIngestor, IndexerCheckpoint, LedgerEntry, Transaction};
use sqlx::postgres::PgPoolOptions;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use uuid::Uuid;

struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn internal(e: impl std::fmt::Display) -> Self {
        error!(error = %e, "Internal server error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "Internal server error".to_string(),
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

    fn service_unavailable(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg.into(),
        }
    }

    fn forbidden(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
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

const MAX_CONCURRENT_JOBS: usize = 10;
const MAX_CONCURRENT_STREAMS: usize = 5;

struct AppState {
    repo: Repository,
    config: AppConfig,
    allowed_wallets: Option<HashSet<String>>,
    jobs: RwLock<HashMap<Uuid, JobEntry>>,
    job_semaphore: Arc<Semaphore>,
    streams: RwLock<HashMap<Uuid, StreamEntry>>,
    stream_semaphore: Arc<Semaphore>,
}

struct StreamEntry {
    id: Uuid,
    chain: String,
    wallet: Option<String>,
    cancel: CancellationToken,
    started_at: Instant,
    tx_count: Arc<std::sync::atomic::AtomicU64>,
    last_slot: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Serialize)]
struct StreamInfo {
    id: Uuid,
    chain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wallet: Option<String>,
    uptime_secs: u64,
    transactions_ingested: u64,
    last_slot: u64,
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

    let allowed_wallets = config.allowed_wallets_set();
    let shared_state = Arc::new(AppState {
        repo: Repository::new(pool),
        config: config.clone(),
        allowed_wallets,
        jobs: RwLock::new(HashMap::new()),
        job_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_JOBS)),
        streams: RwLock::new(HashMap::new()),
        stream_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)),
    });

    let protected = Router::new()
        .route("/v1/ingest", post(trigger_ingest))
        .route("/v1/ingest/batch", post(trigger_batch_ingest))
        .route("/v1/normalize", post(trigger_normalize))
        .route("/v1/jobs/{job_id}", get(get_job_status))
        .route("/v1/transactions/{wallet}", get(get_transactions))
        .route("/v1/ledger/{wallet}", get(get_ledger))
        .route("/v1/export/{wallet}", get(export_ledger))
        .route("/v1/balances/{wallet}", get(get_balances))
        .route(
            "/v1/transactions/{wallet}/{tx_hash}",
            get(get_single_transaction),
        )
        .route("/v1/stats/{wallet}", get(get_wallet_stats))
        .route("/v1/stream/start", post(start_stream))
        .route("/v1/stream/{stream_id}/stop", post(stop_stream))
        .route("/v1/streams", get(list_streams))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&shared_state),
            require_auth,
        ));

    let app = Router::new()
        .route("/health", get(health_check))
        .merge(protected)
        .layer(axum::extract::DefaultBodyLimit::max(1_048_576))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(60),
        ))
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
        None => {
            return Err(AppError {
                status: StatusCode::UNAUTHORIZED,
                message: "API key not configured".to_string(),
            })
        }
    };

    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    match header {
        Some(token) if token.as_bytes().ct_eq(expected.as_bytes()).into() => {
            Ok(next.run(req).await)
        }
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
    callback_url: Option<String>,
}

#[derive(Deserialize)]
struct BatchIngestRequest {
    wallets: Vec<IngestRequest>,
}

#[derive(Deserialize)]
struct NormalizeRequest {
    wallet: String,
    callback_url: Option<String>,
}

const DEFAULT_PAGE_LIMIT: i64 = 50;
const MAX_PAGE_LIMIT: i64 = 1000;

#[derive(Deserialize)]
struct PaginationParams {
    limit: Option<i64>,
    offset: Option<i64>,
    from: Option<i64>,
    to: Option<i64>,
}

#[derive(Deserialize)]
struct ExportParams {
    format: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
}

#[derive(Deserialize)]
struct BalanceParams {
    at: Option<i64>,
}

#[derive(Serialize)]
struct AssetBalance {
    asset_symbol: String,
    balance: BigDecimal,
}

#[derive(Serialize)]
struct ChainTxCount {
    chain: String,
    count: i64,
}

#[derive(Serialize)]
struct WalletStats {
    total_transactions: i64,
    earliest_timestamp: Option<i64>,
    latest_timestamp: Option<i64>,
    total_chains: i64,
    unique_assets: i64,
    transactions_per_chain: Vec<ChainTxCount>,
}

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

fn clamp_offset(offset: Option<i64>) -> i64 {
    offset.unwrap_or(0).max(0)
}

fn validate_date_range(from: Option<i64>, to: Option<i64>) -> Result<(), AppError> {
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            return Err(AppError::bad_request("'from' must be <= 'to'"));
        }
    }
    Ok(())
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

async fn fire_callback(url: &str, payload: &serde_json::Value) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, url, "Failed to build callback HTTP client");
            return;
        }
    };
    if let Err(e) = client.post(url).json(payload).send().await {
        warn!(error = %e, url, "Callback request failed");
    }
}

fn is_private_ip(host: &str) -> bool {
    use std::net::IpAddr;
    if let Ok(ip) = host.parse::<IpAddr>() {
        match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_unspecified()
            }
            IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
        }
    } else {
        host == "localhost"
    }
}

fn validate_callback_url(url: &str) -> Result<(), AppError> {
    let parsed: Result<reqwest::Url, _> = url.parse();
    match parsed {
        Ok(u) if u.scheme() == "https" || u.scheme() == "http" => {
            if let Some(host) = u.host_str() {
                if is_private_ip(host) {
                    return Err(AppError::bad_request(
                        "callback_url must not target private/loopback addresses",
                    ));
                }
            }
            Ok(())
        }
        _ => Err(AppError::bad_request(
            "callback_url must be a valid HTTP(S) URL",
        )),
    }
}

fn check_wallet_allowed(wallet: &str, allowed: &Option<HashSet<String>>) -> Result<(), AppError> {
    if let Some(set) = allowed {
        if !set.contains(&wallet.to_lowercase()) {
            return Err(AppError::forbidden("Wallet not in allowed set"));
        }
    }
    Ok(())
}

async fn trigger_ingest(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<IngestRequest>,
) -> Result<Json<JobStatus>, AppError> {
    validate_wallet(&payload.wallet)?;
    check_wallet_allowed(&payload.wallet, &state.allowed_wallets)?;
    if let Some(ref url) = payload.callback_url {
        validate_callback_url(url)?;
    }

    let chain = payload.chain.clone();
    match chain.as_str() {
        "solana" | "ethereum" | "hyperliquid" => {}
        other => {
            return Err(AppError::bad_request(format!(
                "Unsupported chain: {other}. Supported chains: solana, ethereum, hyperliquid"
            )));
        }
    }

    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::service_unavailable("Too many concurrent jobs"))?;

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
    let callback_url = payload.callback_url.clone();
    let limit = state.config.ingest_limit;
    let user_id = payload.user_id.unwrap_or_else(Uuid::new_v4);

    tokio::spawn(async move {
        let _permit = permit;
        {
            let mut jobs = state_clone.jobs.write().await;
            if let Some(entry) = jobs.get_mut(&job_id) {
                entry.status.state = JobState::Running;
            } else {
                warn!(job_id = %job_id, "Job entry missing when setting running state");
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
                    let adapter = EvmAdapter::new(&state_clone.config.evm_rpc_url)?;
                    adapter
                        .fetch_history(&wallet, limit, user_id, checkpoint.as_ref())
                        .await?
                }
                "solana" => {
                    let adapter = SolanaAdapter::new(&state_clone.config.solana_rpc_url);
                    adapter
                        .fetch_history(&wallet, limit, user_id, checkpoint.as_ref())
                        .await?
                }
                _ => unreachable!("chain validated before spawn"),
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

        let (final_state, final_message) = {
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
                let s = serde_json::to_value(&entry.status.state).ok();
                let m = entry.status.message.clone();
                (s, m)
            } else {
                (None, None)
            }
        };

        if let Some(ref url) = callback_url {
            let payload = serde_json::json!({
                "job_id": job_id,
                "state": final_state,
                "wallet": wallet,
                "chain": chain,
                "message": final_message,
            });
            fire_callback(url, &payload).await;
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

const MAX_BATCH_SIZE: usize = 50;

async fn trigger_batch_ingest(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<BatchIngestRequest>,
) -> Result<Json<Vec<JobStatus>>, AppError> {
    if payload.wallets.is_empty() {
        return Err(AppError::bad_request("wallets array must not be empty"));
    }
    if payload.wallets.len() > MAX_BATCH_SIZE {
        return Err(AppError::bad_request(format!(
            "batch size {} exceeds maximum of {MAX_BATCH_SIZE}",
            payload.wallets.len()
        )));
    }

    for item in &payload.wallets {
        validate_wallet(&item.wallet)?;
        check_wallet_allowed(&item.wallet, &state.allowed_wallets)?;
        match item.chain.as_str() {
            "solana" | "ethereum" | "hyperliquid" => {}
            other => {
                return Err(AppError::bad_request(format!(
                    "Unsupported chain: {other}. Supported chains: solana, ethereum, hyperliquid"
                )));
            }
        }
    }

    let mut jobs = Vec::with_capacity(payload.wallets.len());
    for item in payload.wallets {
        let single = Json(IngestRequest {
            chain: item.chain,
            wallet: item.wallet,
            user_id: item.user_id,
            callback_url: item.callback_url,
        });
        match trigger_ingest(State(Arc::clone(&state)), single).await {
            Ok(Json(status)) => jobs.push(status),
            Err(e) if e.status == StatusCode::SERVICE_UNAVAILABLE => {
                return Err(AppError::service_unavailable(format!(
                    "Concurrency limit reached after queuing {} of {} jobs",
                    jobs.len(),
                    jobs.len() + 1
                )));
            }
            Err(e) => return Err(e),
        }
    }

    Ok(Json(jobs))
}

async fn trigger_normalize(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<NormalizeRequest>,
) -> Result<Json<JobStatus>, AppError> {
    validate_wallet(&payload.wallet)?;
    check_wallet_allowed(&payload.wallet, &state.allowed_wallets)?;
    if let Some(ref url) = payload.callback_url {
        validate_callback_url(url)?;
    }

    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::service_unavailable("Too many concurrent jobs"))?;

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
    let callback_url = payload.callback_url.clone();

    tokio::spawn(async move {
        let _permit = permit;
        {
            let mut jobs = state_clone.jobs.write().await;
            if let Some(entry) = jobs.get_mut(&job_id) {
                entry.status.state = JobState::Running;
            } else {
                warn!(job_id = %job_id, "Job entry missing when setting running state");
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

        let (final_state, final_message) = {
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
                let s = serde_json::to_value(&entry.status.state).ok();
                let m = entry.status.message.clone();
                (s, m)
            } else {
                (None, None)
            }
        };

        if let Some(ref url) = callback_url {
            let payload = serde_json::json!({
                "job_id": job_id,
                "state": final_state,
                "wallet": wallet,
                "message": final_message,
            });
            fire_callback(url, &payload).await;
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
    check_wallet_allowed(&wallet, &state.allowed_wallets)?;
    validate_date_range(params.from, params.to)?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let txs = state
        .repo
        .get_transactions_by_wallet_filtered(&wallet, limit, offset, params.from, params.to)
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
    check_wallet_allowed(&wallet, &state.allowed_wallets)?;
    validate_date_range(params.from, params.to)?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let entries = state
        .repo
        .get_ledger_entries_by_wallet_filtered(&wallet, limit, offset, params.from, params.to)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(entries))
}

const MAX_EXPORT_LIMIT: i64 = 10_000;

fn format_entry_type(et: &spectraplex_core::models::EntryType) -> &'static str {
    match et {
        spectraplex_core::models::EntryType::Trade => "trade",
        spectraplex_core::models::EntryType::Fee => "fee",
        spectraplex_core::models::EntryType::Transfer => "transfer",
        spectraplex_core::models::EntryType::Staking => "staking",
        spectraplex_core::models::EntryType::Income => "income",
    }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

async fn export_ledger(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
    Query(params): Query<ExportParams>,
) -> Result<Response, AppError> {
    validate_wallet(&wallet)?;
    check_wallet_allowed(&wallet, &state.allowed_wallets)?;
    validate_date_range(params.from, params.to)?;

    let format = params.format.as_deref().unwrap_or("json");
    if format != "csv" && format != "json" {
        return Err(AppError::bad_request(format!(
            "Unsupported format: {format}. Use 'csv' or 'json'"
        )));
    }

    let entries = state
        .repo
        .get_ledger_entries_by_wallet_filtered(&wallet, MAX_EXPORT_LIMIT, 0, params.from, params.to)
        .await
        .map_err(AppError::internal)?;

    match format {
        "csv" => {
            let mut buf = String::from(
                "id,transaction_id,wallet_address,asset_symbol,amount,entry_type,fiat_value\n",
            );
            for e in &entries {
                let fiat = e
                    .fiat_value
                    .as_ref()
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                buf.push_str(&format!(
                    "{},{},{},{},{},{},{}\n",
                    e.id,
                    e.transaction_id,
                    csv_escape(&e.wallet_address),
                    csv_escape(&e.asset_symbol),
                    e.amount,
                    format_entry_type(&e.entry_type),
                    fiat,
                ));
            }
            Ok((
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/csv; charset=utf-8")],
                buf,
            )
                .into_response())
        }
        "json" => {
            let json = serde_json::to_string(&entries).map_err(AppError::internal)?;
            Ok((
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                json,
            )
                .into_response())
        }
        _ => unreachable!("format validated before query"),
    }
}

async fn get_balances(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
    Query(params): Query<BalanceParams>,
) -> Result<Json<Vec<AssetBalance>>, AppError> {
    validate_wallet(&wallet)?;
    check_wallet_allowed(&wallet, &state.allowed_wallets)?;
    let rows = state
        .repo
        .get_balances(&wallet, params.at)
        .await
        .map_err(AppError::internal)?;
    let balances: Vec<AssetBalance> = rows
        .into_iter()
        .map(|(asset_symbol, balance)| AssetBalance {
            asset_symbol,
            balance,
        })
        .collect();
    Ok(Json(balances))
}

async fn get_single_transaction(
    State(state): State<Arc<AppState>>,
    Path((wallet, tx_hash)): Path<(String, String)>,
) -> Result<Json<Transaction>, AppError> {
    validate_wallet(&wallet)?;
    check_wallet_allowed(&wallet, &state.allowed_wallets)?;

    let tx = state
        .repo
        .get_transaction_by_hash(&wallet, &tx_hash)
        .await
        .map_err(AppError::internal)?;

    match tx {
        Some(tx) => Ok(Json(tx)),
        None => Err(AppError::not_found("Transaction not found")),
    }
}

async fn get_wallet_stats(
    State(state): State<Arc<AppState>>,
    Path(wallet): Path<String>,
) -> Result<Json<WalletStats>, AppError> {
    validate_wallet(&wallet)?;
    check_wallet_allowed(&wallet, &state.allowed_wallets)?;

    let stats = state
        .repo
        .get_wallet_stats(&wallet)
        .await
        .map_err(AppError::internal)?;

    Ok(Json(WalletStats {
        total_transactions: stats.tx_count,
        earliest_timestamp: stats.earliest_timestamp,
        latest_timestamp: stats.latest_timestamp,
        total_chains: stats.chain_count,
        unique_assets: stats.unique_assets,
        transactions_per_chain: stats
            .per_chain
            .into_iter()
            .map(|(chain, count)| ChainTxCount { chain, count })
            .collect(),
    }))
}

#[derive(Deserialize)]
struct StartStreamRequest {
    chain: String,
    wallet: Option<String>,
}

async fn start_stream(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<StartStreamRequest>,
) -> Result<Json<StreamInfo>, AppError> {
    match payload.chain.as_str() {
        "solana" => {
            if state
                .config
                .solana_grpc_url
                .as_deref()
                .filter(|u| !u.is_empty())
                .is_none()
            {
                return Err(AppError::bad_request(
                    "Solana gRPC URL not configured (set SOLANA_GRPC_URL)",
                ));
            }
        }
        "hyperliquid" => {
            let wallet = payload.wallet.as_deref().unwrap_or("");
            validate_wallet(wallet)?;
            check_wallet_allowed(wallet, &state.allowed_wallets)?;
        }
        other => {
            return Err(AppError::bad_request(format!(
                "Unsupported streaming chain: {other}. Supported: solana, hyperliquid"
            )));
        }
    }

    let _permit = state
        .stream_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::service_unavailable("Too many concurrent streams"))?;

    let stream_id = Uuid::new_v4();
    let cancel = CancellationToken::new();
    let tx_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let last_slot = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let chain = payload.chain.clone();
    let wallet = payload.wallet.clone();

    let cancel_clone = cancel.clone();
    let tx_count_clone = Arc::clone(&tx_count);
    let last_slot_clone = Arc::clone(&last_slot);
    let repo = state.repo.clone();
    let state_clone = Arc::clone(&state);

    match chain.as_str() {
        "solana" => {
            let grpc_url = state.config.solana_grpc_url.clone().unwrap();
            let grpc_token = state.config.solana_grpc_token.clone();
            let adapter = SolanaGrpcAdapter::new(&grpc_url, grpc_token);
            let (mut rx, grpc_handle) = adapter.stream_transactions();

            tokio::spawn(async move {
                let _permit = _permit;
                let mut batch: Vec<Transaction> = Vec::new();
                let mut last_flush = Instant::now();
                const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
                const BATCH_SIZE: usize = 100;

                loop {
                    tokio::select! {
                        _ = cancel_clone.cancelled() => {
                            info!(stream_id = %stream_id, "Stream cancelled");
                            break;
                        }
                        maybe_tx = rx.recv() => {
                            match maybe_tx {
                                Some(tx) => {
                                    if let Some(slot) = tx.raw_metadata.get("slot").and_then(|v| v.as_u64()) {
                                        last_slot_clone.store(slot, std::sync::atomic::Ordering::Relaxed);
                                    }
                                    batch.push(tx);
                                    tx_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                                    if batch.len() >= BATCH_SIZE || last_flush.elapsed() >= FLUSH_INTERVAL {
                                        if let Err(e) = repo.save_transactions(&batch).await {
                                            error!(stream_id = %stream_id, error = %e, "Failed to save streamed transactions");
                                        }
                                        batch.clear();
                                        last_flush = Instant::now();
                                    }
                                }
                                None => {
                                    info!(stream_id = %stream_id, "gRPC stream channel closed");
                                    break;
                                }
                            }
                        }
                    }
                }

                if !batch.is_empty() {
                    if let Err(e) = repo.save_transactions(&batch).await {
                        error!(stream_id = %stream_id, error = %e, "Failed to flush final batch");
                    }
                }

                grpc_handle.abort();
                state_clone.streams.write().await.remove(&stream_id);
                info!(stream_id = %stream_id, "Stream removed from active set");
            });
        }
        "hyperliquid" => {
            let hl_wallet = wallet.clone().unwrap();
            let (sender, mut receiver) = tokio::sync::mpsc::channel::<serde_json::Value>(1000);

            let ws_cancel = cancel_clone.clone();
            let ws_wallet = hl_wallet.clone();
            let ws_handle = tokio::spawn(async move {
                let client = HyperliquidWsClient::new();
                tokio::select! {
                    result = client.subscribe_user(&ws_wallet, |msg| {
                        if let Some(data) = msg.data {
                            let _ = sender.try_send(data);
                        }
                    }) => {
                        if let Err(e) = result {
                            error!(error = %e, "Hyperliquid WebSocket error");
                        }
                    }
                    _ = ws_cancel.cancelled() => {
                        info!("Hyperliquid stream cancelled");
                    }
                }
            });

            let hl_wallet_owned = hl_wallet.clone();
            tokio::spawn(async move {
                let _permit = _permit;
                let mut batch: Vec<Transaction> = Vec::new();
                let mut last_flush = Instant::now();
                let user_id = Uuid::new_v4();
                const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
                const BATCH_SIZE: usize = 100;

                loop {
                    tokio::select! {
                        _ = cancel_clone.cancelled() => {
                            info!(stream_id = %stream_id, "Hyperliquid stream cancelled");
                            break;
                        }
                        msg = receiver.recv() => {
                            match msg {
                                Some(data) => {
                                    let tx = Transaction {
                                        id: Uuid::new_v4(),
                                        user_id,
                                        wallet_address: hl_wallet_owned.clone(),
                                        timestamp: chrono::Utc::now().timestamp(),
                                        tx_hash: format!("hl-ws-{}", Uuid::new_v4()),
                                        chain: spectraplex_core::models::Chain::Hyperliquid,
                                        raw_metadata: data,
                                    };
                                    batch.push(tx);
                                    tx_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                                    if batch.len() >= BATCH_SIZE || last_flush.elapsed() >= FLUSH_INTERVAL {
                                        if let Err(e) = repo.save_transactions(&batch).await {
                                            error!(stream_id = %stream_id, error = %e, "Failed to save HL stream batch");
                                        }
                                        batch.clear();
                                        last_flush = Instant::now();
                                    }
                                }
                                None => {
                                    info!(stream_id = %stream_id, "Hyperliquid WS channel closed");
                                    break;
                                }
                            }
                        }
                    }
                }

                if !batch.is_empty() {
                    if let Err(e) = repo.save_transactions(&batch).await {
                        error!(stream_id = %stream_id, error = %e, "Failed to flush final HL batch");
                    }
                }

                ws_handle.abort();
                state_clone.streams.write().await.remove(&stream_id);
                info!(stream_id = %stream_id, "Hyperliquid stream removed from active set");
            });
        }
        _ => unreachable!("chain validated above"),
    }

    let entry = StreamEntry {
        id: stream_id,
        chain: chain.clone(),
        wallet: wallet.clone(),
        cancel: cancel.clone(),
        started_at: Instant::now(),
        tx_count: Arc::clone(&tx_count),
        last_slot: Arc::clone(&last_slot),
    };

    let info = StreamInfo {
        id: stream_id,
        chain,
        wallet,
        uptime_secs: 0,
        transactions_ingested: 0,
        last_slot: 0,
    };

    state.streams.write().await.insert(stream_id, entry);
    info!(stream_id = %stream_id, "Stream started");

    Ok(Json(info))
}

async fn stop_stream(
    State(state): State<Arc<AppState>>,
    Path(stream_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let streams = state.streams.read().await;
    let entry = streams
        .get(&stream_id)
        .ok_or_else(|| AppError::not_found(format!("Stream {} not found", stream_id)))?;
    entry.cancel.cancel();
    drop(streams);

    Ok(Json(serde_json::json!({
        "id": stream_id,
        "status": "stopping"
    })))
}

async fn list_streams(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<StreamInfo>>, AppError> {
    let streams = state.streams.read().await;
    let infos: Vec<StreamInfo> = streams
        .values()
        .map(|entry| StreamInfo {
            id: entry.id,
            chain: entry.chain.clone(),
            wallet: entry.wallet.clone(),
            uptime_secs: entry.started_at.elapsed().as_secs(),
            transactions_ingested: entry.tx_count.load(std::sync::atomic::Ordering::Relaxed),
            last_slot: entry.last_slot.load(std::sync::atomic::Ordering::Relaxed),
        })
        .collect();
    Ok(Json(infos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use spectraplex_core::models::Chain;
    use tower::ServiceExt;

    const TEST_API_KEY: &str = "test-api-key";

    fn test_state() -> Arc<AppState> {
        test_state_with_key(Some(TEST_API_KEY.to_string()))
    }

    fn test_state_with_key(api_key: Option<String>) -> Arc<AppState> {
        test_state_with_config(api_key, None)
    }

    fn test_state_with_config(
        api_key: Option<String>,
        allowed_wallets: Option<String>,
    ) -> Arc<AppState> {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fake:fake@localhost/fake")
            .unwrap();
        let config = AppConfig {
            api_key,
            allowed_wallets,
            ..AppConfig::default()
        };
        let allowed_wallets_set = config.allowed_wallets_set();
        Arc::new(AppState {
            repo: Repository::new(pool),
            config,
            allowed_wallets: allowed_wallets_set,
            jobs: RwLock::new(HashMap::new()),
            job_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_JOBS)),
            streams: RwLock::new(HashMap::new()),
            stream_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)),
        })
    }

    fn test_router() -> Router {
        let state = test_state();
        test_router_with_state(state)
    }

    fn test_router_with_state(state: Arc<AppState>) -> Router {
        let protected = Router::new()
            .route("/v1/ingest", post(trigger_ingest))
            .route("/v1/ingest/batch", post(trigger_batch_ingest))
            .route("/v1/normalize", post(trigger_normalize))
            .route("/v1/jobs/{job_id}", get(get_job_status))
            .route("/v1/transactions/{wallet}", get(get_transactions))
            .route("/v1/ledger/{wallet}", get(get_ledger))
            .route("/v1/export/{wallet}", get(export_ledger))
            .route("/v1/balances/{wallet}", get(get_balances))
            .route(
                "/v1/transactions/{wallet}/{tx_hash}",
                get(get_single_transaction),
            )
            .route("/v1/stats/{wallet}", get(get_wallet_stats))
            .route("/v1/stream/start", post(start_stream))
            .route("/v1/stream/{stream_id}/stop", post(stop_stream))
            .route("/v1/streams", get(list_streams))
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
    fn test_app_error_internal_hides_details() {
        let err = AppError::internal("database connection refused on port 5432");
        assert_eq!(err.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.message, "Internal server error");
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
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
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
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
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
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
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
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
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
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
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
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
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
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
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
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
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
    async fn test_auth_rejects_when_no_key_configured() {
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
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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

    #[tokio::test]
    async fn test_ingest_unsupported_chain() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "bitcoin",
                    "wallet": "abc123"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("Unsupported chain"));
    }

    #[tokio::test]
    async fn test_ingest_valid_chains_accepted() {
        for chain in &["solana", "ethereum", "hyperliquid"] {
            let app = test_router();
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/v1/ingest")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", TEST_API_KEY))
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "chain": chain,
                        "wallet": "abc123"
                    }))
                    .unwrap(),
                ))
                .unwrap();
            let response = app.oneshot(req).await.unwrap();
            assert_ne!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "chain {chain} should be accepted"
            );
        }
    }

    #[tokio::test]
    async fn test_semaphore_limits_concurrent_jobs() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fake:fake@localhost/fake")
            .unwrap();
        let config = AppConfig {
            api_key: Some("secret".to_string()),
            ..AppConfig::default()
        };
        let state = Arc::new(AppState {
            repo: Repository::new(pool),
            config,
            allowed_wallets: None,
            jobs: RwLock::new(HashMap::new()),
            job_semaphore: Arc::new(Semaphore::new(1)),
            streams: RwLock::new(HashMap::new()),
            stream_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)),
        });
        let app = test_router_with_state(Arc::clone(&state));

        let req1 = axum::http::Request::builder()
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
        let resp1 = app.oneshot(req1).await.unwrap();
        assert_ne!(resp1.status(), StatusCode::SERVICE_UNAVAILABLE);

        tokio::time::sleep(Duration::from_millis(50)).await;

        let app2 = test_router_with_state(Arc::clone(&state));
        let req2 = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "def456"
                }))
                .unwrap(),
            ))
            .unwrap();
        let resp2 = app2.oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_app_error_service_unavailable() {
        let err = AppError::service_unavailable("Too many concurrent jobs");
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.message, "Too many concurrent jobs");
    }

    #[tokio::test]
    async fn test_balances_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/balances/bad%20wallet")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_balances_endpoint_exists() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/balances/abc123")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn test_asset_balance_serialization() {
        let balance = AssetBalance {
            asset_symbol: "SOL".to_string(),
            balance: bigdecimal::BigDecimal::from(42),
        };
        let json = serde_json::to_value(&balance).unwrap();
        assert_eq!(json["asset_symbol"], "SOL");
        assert_eq!(json["balance"], "42");
    }

    #[tokio::test]
    async fn test_balances_respects_wallet_scoping() {
        let state = test_state_with_config(Some("secret".to_string()), Some("abc123".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/v1/balances/notallowed")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
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

    #[test]
    fn test_check_wallet_allowed_no_restriction() {
        assert!(check_wallet_allowed("abc123", &None).is_ok());
    }

    #[test]
    fn test_check_wallet_allowed_permitted() {
        let allowed: Option<HashSet<String>> = Some(["abc123".to_string()].into_iter().collect());
        assert!(check_wallet_allowed("abc123", &allowed).is_ok());
    }

    #[test]
    fn test_check_wallet_allowed_case_insensitive() {
        let allowed: Option<HashSet<String>> = Some(["abc123".to_string()].into_iter().collect());
        assert!(check_wallet_allowed("ABC123", &allowed).is_ok());
    }

    #[test]
    fn test_check_wallet_allowed_denied() {
        let allowed: Option<HashSet<String>> = Some(["abc123".to_string()].into_iter().collect());
        let err = check_wallet_allowed("xyz789", &allowed).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_wallet_scoping_allows_permitted_wallet() {
        let state = test_state_with_config(
            Some("secret".to_string()),
            Some("abc123,xyz789".to_string()),
        );
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/abc123")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_wallet_scoping_rejects_unpermitted_wallet() {
        let state = test_state_with_config(Some("secret".to_string()), Some("abc123".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/notallowed")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_wallet_scoping_allows_all_when_not_configured() {
        let state = test_state_with_config(Some("secret".to_string()), None);
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/anywallet")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_app_error_forbidden() {
        let err = AppError::forbidden("not allowed");
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert_eq!(err.message, "not allowed");
    }

    #[tokio::test]
    async fn test_batch_ingest_multiple_wallets() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest/batch")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallets": [
                        {"chain": "solana", "wallet": "abc123"},
                        {"chain": "ethereum", "wallet": "0xdef456"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let jobs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0]["state"], "pending");
        assert_eq!(jobs[1]["state"], "pending");
    }

    #[tokio::test]
    async fn test_batch_ingest_empty_wallets() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest/batch")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallets": []
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_batch_ingest_invalid_chain() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest/batch")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallets": [
                        {"chain": "bitcoin", "wallet": "abc123"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_batch_ingest_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest/batch")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallets": [
                        {"chain": "solana", "wallet": "bad;wallet"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_batch_ingest_respects_wallet_scoping() {
        let state = test_state_with_config(Some("secret".to_string()), Some("abc123".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest/batch")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallets": [
                        {"chain": "solana", "wallet": "notallowed"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_batch_ingest_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest/batch")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallets": [
                        {"chain": "solana", "wallet": "abc123"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_export_json_passes_validation() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/export/abc123")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        // With a fake DB pool the query fails, but format validation passes (not 400)
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_callback_url_valid_https() {
        assert!(validate_callback_url("https://example.com/webhook").is_ok());
    }

    #[test]
    fn test_validate_callback_url_valid_http() {
        assert!(validate_callback_url("http://example.com/callback").is_ok());
    }

    #[test]
    fn test_validate_callback_url_invalid() {
        let err = validate_callback_url("not-a-url").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_callback_url_ftp_rejected() {
        let err = validate_callback_url("ftp://example.com/file").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_callback_url_loopback_rejected() {
        let err = validate_callback_url("http://127.0.0.1:8080/hook").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_callback_url_localhost_rejected() {
        let err = validate_callback_url("https://localhost/hook").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_callback_url_private_ip_rejected() {
        assert!(validate_callback_url("http://10.0.0.1/hook").is_err());
        assert!(validate_callback_url("http://172.16.0.1/hook").is_err());
        assert!(validate_callback_url("http://192.168.1.1/hook").is_err());
    }

    #[test]
    fn test_is_private_ip() {
        assert!(is_private_ip("127.0.0.1"));
        assert!(is_private_ip("10.0.0.1"));
        assert!(is_private_ip("172.16.0.1"));
        assert!(is_private_ip("192.168.1.1"));
        assert!(is_private_ip("169.254.1.1"));
        assert!(is_private_ip("localhost"));
        assert!(is_private_ip("::1"));
        assert!(!is_private_ip("8.8.8.8"));
        assert!(!is_private_ip("example.com"));
    }

    #[tokio::test]
    async fn test_ingest_with_callback_url_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123",
                    "callback_url": "https://example.com/webhook"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_csv_passes_validation() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/export/abc123?format=csv")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        // With a fake DB pool the query fails, but format validation passes (not 400)
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_unsupported_format() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/export/abc123?format=xml")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_ingest_with_invalid_callback_url_rejected() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123",
                    "callback_url": "not-a-url"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/export/abc123")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_export_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/export/bad%20wallet")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_normalize_with_callback_url_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/normalize")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallet": "abc123",
                    "callback_url": "https://example.com/webhook"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_normalize_with_invalid_callback_url_rejected() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/normalize")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "wallet": "abc123",
                    "callback_url": "ftp://bad"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_respects_wallet_scoping() {
        let state = test_state_with_config(Some("secret".to_string()), Some("abc123".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/v1/export/notallowed")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_csv_escape_plain() {
        assert_eq!(csv_escape("hello"), "hello");
    }

    #[test]
    fn test_csv_escape_comma() {
        assert_eq!(csv_escape("hello,world"), "\"hello,world\"");
    }

    #[test]
    fn test_csv_escape_quotes() {
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn test_format_entry_type_all_variants() {
        use spectraplex_core::models::EntryType;
        assert_eq!(format_entry_type(&EntryType::Trade), "trade");
        assert_eq!(format_entry_type(&EntryType::Fee), "fee");
        assert_eq!(format_entry_type(&EntryType::Transfer), "transfer");
        assert_eq!(format_entry_type(&EntryType::Staking), "staking");
        assert_eq!(format_entry_type(&EntryType::Income), "income");
    }

    #[test]
    fn test_validate_date_range_both_none() {
        assert!(validate_date_range(None, None).is_ok());
    }

    #[test]
    fn test_validate_date_range_only_from() {
        assert!(validate_date_range(Some(1000), None).is_ok());
    }

    #[test]
    fn test_validate_date_range_only_to() {
        assert!(validate_date_range(None, Some(2000)).is_ok());
    }

    #[test]
    fn test_validate_date_range_valid() {
        assert!(validate_date_range(Some(1000), Some(2000)).is_ok());
    }

    #[test]
    fn test_validate_date_range_equal() {
        assert!(validate_date_range(Some(1000), Some(1000)).is_ok());
    }

    #[test]
    fn test_validate_date_range_invalid() {
        let err = validate_date_range(Some(2000), Some(1000)).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_transactions_with_date_range_params() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/abc123?from=1700000000&to=1700100000")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_ingest_with_loopback_callback_rejected() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123",
                    "callback_url": "http://127.0.0.1:9999/hook"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_ingest_without_callback_url_still_works() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/ingest")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "solana",
                    "wallet": "abc123"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_transactions_invalid_date_range() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/abc123?from=2000&to=1000")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_ledger_with_date_range_params() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/ledger/abc123?from=1700000000&to=1700100000")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_ledger_invalid_date_range() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/ledger/abc123?from=2000&to=1000")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_with_date_range_params() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/export/abc123?format=csv&from=1700000000&to=1700100000")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_invalid_date_range() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/export/abc123?from=2000&to=1000")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_single_transaction_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/bad%20wallet/0xabc")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_single_transaction_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/abc123/0xdeadbeef")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_single_transaction_respects_wallet_scoping() {
        let state = test_state_with_config(Some("secret".to_string()), Some("abc123".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/notallowed/0xabc")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_single_transaction_passes_validation() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/abc123/0xdeadbeef")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_stats_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/stats/bad%20wallet")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_stats_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/stats/abc123")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_stats_respects_wallet_scoping() {
        let state = test_state_with_config(Some("secret".to_string()), Some("abc123".to_string()));
        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .uri("/v1/stats/notallowed")
            .header("authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_stats_passes_validation() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/stats/abc123")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
        assert_ne!(response.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_wallet_stats_serialization() {
        let stats = WalletStats {
            total_transactions: 42,
            earliest_timestamp: Some(1700000000),
            latest_timestamp: Some(1700100000),
            total_chains: 2,
            unique_assets: 5,
            transactions_per_chain: vec![
                ChainTxCount {
                    chain: "ethereum".to_string(),
                    count: 20,
                },
                ChainTxCount {
                    chain: "solana".to_string(),
                    count: 22,
                },
            ],
        };
        let json = serde_json::to_value(&stats).unwrap();
        assert_eq!(json["total_transactions"], 42);
        assert_eq!(json["earliest_timestamp"], 1700000000);
        assert_eq!(json["latest_timestamp"], 1700100000);
        assert_eq!(json["total_chains"], 2);
        assert_eq!(json["unique_assets"], 5);
        assert_eq!(json["transactions_per_chain"].as_array().unwrap().len(), 2);
        assert_eq!(json["transactions_per_chain"][0]["chain"], "ethereum");
        assert_eq!(json["transactions_per_chain"][0]["count"], 20);
    }

    #[tokio::test]
    async fn test_stream_start_unsupported_chain() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/stream/start")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({"chain": "ethereum"})).unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("Unsupported streaming chain"));
    }

    #[tokio::test]
    async fn test_stream_start_no_grpc_url_configured() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/stream/start")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({"chain": "solana"})).unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("gRPC URL not configured"));
    }

    #[tokio::test]
    async fn test_stream_start_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/stream/start")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({"chain": "solana"})).unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_stream_stop_not_found() {
        let app = test_router();
        let stream_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/v1/stream/{}/stop", stream_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_stream_stop_requires_auth() {
        let app = test_router();
        let stream_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/v1/stream/{}/stop", stream_id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_list_streams_empty() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/streams")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let streams: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(streams.is_empty());
    }

    #[tokio::test]
    async fn test_list_streams_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/streams")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_stream_stop_with_active_stream() {
        let state = test_state();
        let stream_id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        state.streams.write().await.insert(
            stream_id,
            StreamEntry {
                id: stream_id,
                chain: "solana".to_string(),
                wallet: None,
                cancel: cancel.clone(),
                started_at: Instant::now(),
                tx_count: Arc::new(std::sync::atomic::AtomicU64::new(42)),
                last_slot: Arc::new(std::sync::atomic::AtomicU64::new(12345)),
            },
        );

        let app = test_router_with_state(Arc::clone(&state));
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/v1/stream/{}/stop", stream_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "stopping");

        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn test_list_streams_with_active_stream() {
        let state = test_state();
        let stream_id = Uuid::new_v4();
        state.streams.write().await.insert(
            stream_id,
            StreamEntry {
                id: stream_id,
                chain: "solana".to_string(),
                wallet: None,
                cancel: CancellationToken::new(),
                started_at: Instant::now(),
                tx_count: Arc::new(std::sync::atomic::AtomicU64::new(100)),
                last_slot: Arc::new(std::sync::atomic::AtomicU64::new(50000)),
            },
        );

        let app = test_router_with_state(Arc::clone(&state));
        let req = axum::http::Request::builder()
            .uri("/v1/streams")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let streams: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0]["id"], stream_id.to_string());
        assert_eq!(streams[0]["chain"], "solana");
        assert_eq!(streams[0]["transactions_ingested"], 100);
        assert_eq!(streams[0]["last_slot"], 50000);
    }

    #[tokio::test]
    async fn test_stream_semaphore_limits_concurrent_streams() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://fake:fake@localhost/fake")
            .unwrap();
        let config = AppConfig {
            api_key: Some("secret".to_string()),
            solana_grpc_url: Some("http://fake-grpc:10000".to_string()),
            ..AppConfig::default()
        };
        let allowed_wallets_set = config.allowed_wallets_set();
        let state = Arc::new(AppState {
            repo: Repository::new(pool),
            config,
            allowed_wallets: allowed_wallets_set,
            jobs: RwLock::new(HashMap::new()),
            job_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_JOBS)),
            streams: RwLock::new(HashMap::new()),
            stream_semaphore: Arc::new(Semaphore::new(0)),
        });

        let app = test_router_with_state(state);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/stream/start")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({"chain": "solana"})).unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_stream_info_serialization() {
        let info = StreamInfo {
            id: Uuid::nil(),
            chain: "solana".to_string(),
            wallet: None,
            uptime_secs: 120,
            transactions_ingested: 5000,
            last_slot: 300000,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["chain"], "solana");
        assert!(json.get("wallet").is_none());
        assert_eq!(json["uptime_secs"], 120);
        assert_eq!(json["transactions_ingested"], 5000);
        assert_eq!(json["last_slot"], 300000);

        let info_hl = StreamInfo {
            id: Uuid::nil(),
            chain: "hyperliquid".to_string(),
            wallet: Some("0xabc123".to_string()),
            uptime_secs: 60,
            transactions_ingested: 10,
            last_slot: 0,
        };
        let json_hl = serde_json::to_value(&info_hl).unwrap();
        assert_eq!(json_hl["chain"], "hyperliquid");
        assert_eq!(json_hl["wallet"], "0xabc123");
    }

    #[tokio::test]
    async fn test_stream_start_hyperliquid_missing_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/stream/start")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({"chain": "hyperliquid"})).unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("Invalid wallet address length"));
    }

    #[tokio::test]
    async fn test_stream_start_hyperliquid_invalid_wallet() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/stream/start")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "chain": "hyperliquid",
                    "wallet": "bad wallet!@#"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"]
            .as_str()
            .unwrap()
            .contains("invalid characters"));
    }

    #[tokio::test]
    async fn test_list_streams_includes_chain_and_wallet() {
        let state = test_state();
        let stream_id = Uuid::new_v4();
        state.streams.write().await.insert(
            stream_id,
            StreamEntry {
                id: stream_id,
                chain: "hyperliquid".to_string(),
                wallet: Some("0xabc123".to_string()),
                cancel: CancellationToken::new(),
                started_at: Instant::now(),
                tx_count: Arc::new(std::sync::atomic::AtomicU64::new(5)),
                last_slot: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            },
        );

        let app = test_router_with_state(Arc::clone(&state));
        let req = axum::http::Request::builder()
            .uri("/v1/streams")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let streams: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0]["chain"], "hyperliquid");
        assert_eq!(streams[0]["wallet"], "0xabc123");
        assert_eq!(streams[0]["transactions_ingested"], 5);
    }
}
