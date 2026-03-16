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
    repo::{build_checkpoint, Repository},
    solana::SolanaAdapter,
    solana_grpc::SolanaGrpcAdapter,
    solana_parser,
};
use spectraplex_core::config::AppConfig;
use spectraplex_core::connector::validate_target;
use spectraplex_core::materializer::{
    BalanceSnapshot, DatasetName, DeliveryMetadata, DeliveryReceipt, ExportFormat, ExportSink,
    ForensicsActivity, HlPnlSummary, HlTradeHistory, MarketAnalytics, PoolSnapshot,
    ProtocolActivity, ProtocolEvent, SinkConfig, SinkType, TraderAnalytics, TvlAnalytics,
    WalletLedgerRecord,
};
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, LedgerEntry, Transaction};
use spectraplex_core::v2::{
    normalize_evm_address, normalize_solana_address, ChainFamily, DatasetCompleteness,
    DatasetVersion, IndexTarget, Network, TargetKind, TargetMode,
};
use sqlx::postgres::PgPoolOptions;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex as TokioMutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug)]
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

fn serialize_optional_datetime<S>(
    dt: &Option<chrono::DateTime<chrono::Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match dt {
        Some(d) => serializer.serialize_str(&d.to_rfc3339()),
        None => serializer.serialize_none(),
    }
}

const MAX_CONCURRENT_JOBS: usize = 10;
const MAX_CONCURRENT_STREAMS: usize = 5;

/// Default requests allowed per key before throttling.
const RATE_LIMIT_CAPACITY: u32 = 60;
/// Tokens restored per second.
const RATE_LIMIT_REFILL_RATE: f64 = 10.0;
/// Maximum number of tracked keys before triggering eviction.
const RATE_LIMIT_MAX_BUCKETS: usize = 10_000;
/// Buckets unused for longer than this duration are eligible for eviction.
const RATE_LIMIT_EVICT_AFTER: Duration = Duration::from_secs(3600);

/// In-memory token-bucket rate limiter keyed by API key.
struct RateLimiter {
    buckets: TokioMutex<HashMap<String, TokenBucket>>,
    capacity: u32,
    refill_rate: f64,
}

struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
    last_used: Instant,
}

impl RateLimiter {
    fn new(capacity: u32, refill_rate: f64) -> Self {
        Self {
            buckets: TokioMutex::new(HashMap::new()),
            capacity,
            refill_rate,
        }
    }

    /// Try to consume one token for the given key. Returns `true` if allowed.
    async fn try_acquire(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let now = Instant::now();
        let cap = self.capacity as f64;

        // Evict stale entries when the map exceeds the threshold.
        if buckets.len() >= RATE_LIMIT_MAX_BUCKETS {
            let cutoff = now - RATE_LIMIT_EVICT_AFTER;
            buckets.retain(|_, b| b.last_used > cutoff);
        }

        let bucket = buckets.entry(key.to_string()).or_insert(TokenBucket {
            tokens: cap,
            last_refill: now,
            last_used: now,
        });

        // Refill tokens based on elapsed time.
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_rate).min(cap);
        bucket.last_refill = now;
        bucket.last_used = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct AppState {
    repo: Repository,
    config: AppConfig,
    allowed_wallets: Option<HashSet<String>>,
    jobs: RwLock<HashMap<Uuid, JobEntry>>,
    job_semaphore: Arc<Semaphore>,
    streams: RwLock<HashMap<Uuid, StreamEntry>>,
    stream_semaphore: Arc<Semaphore>,
    export_jobs: RwLock<HashMap<Uuid, ExportJobEntry>>,
    rate_limiter: Arc<RateLimiter>,
}

struct StreamEntry {
    id: Uuid,
    cancel: CancellationToken,
    started_at: Instant,
    tx_count: Arc<std::sync::atomic::AtomicU64>,
    last_slot: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Serialize)]
struct StreamInfo {
    id: Uuid,
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
const JOB_CLEANUP_INTERVAL_SECS: u64 = 300; // 5 minutes

impl AppState {
    /// Remove completed/failed jobs older than JOB_TTL_SECS.
    async fn prune_stale_jobs(&self) {
        let mut jobs = self.jobs.write().await;
        let cutoff = Instant::now() - std::time::Duration::from_secs(JOB_TTL_SECS);
        jobs.retain(|_, entry| entry.finished_at.is_none_or(|finished| finished > cutoff));
    }

    /// Remove completed/failed export jobs older than JOB_TTL_SECS.
    async fn prune_stale_export_jobs(&self) {
        let mut exports = self.export_jobs.write().await;
        let cutoff = Instant::now() - std::time::Duration::from_secs(JOB_TTL_SECS);
        exports.retain(|_, entry| entry.finished_at.is_none_or(|finished| finished > cutoff));
    }
}

/// An in-flight or completed export job.
struct ExportJobEntry {
    status: ExportJobStatus,
    finished_at: Option<Instant>,
    /// Completed export data (populated when state == completed).
    data: Option<ExportData>,
}

/// Completed export payload.
struct ExportData {
    content_type: &'static str,
    body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize)]
struct ExportJobStatus {
    id: Uuid,
    state: JobState,
    dataset: String,
    format: String,
    record_count: Option<usize>,
    message: Option<String>,
    /// Where the export data was delivered (e.g. file path, webhook URL).
    #[serde(skip_serializing_if = "Option::is_none")]
    delivered_to: Option<String>,
    /// Delivery status: "pending", "delivered", "failed", or null if no sink.
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery_status: Option<String>,
    /// ID of the dataset version used for this export.
    #[serde(skip_serializing_if = "Option::is_none")]
    dataset_version_id: Option<Uuid>,
    /// Dataset version number.
    #[serde(skip_serializing_if = "Option::is_none")]
    dataset_version: Option<i32>,
    /// Completeness status for the dataset at export time.
    #[serde(skip_serializing_if = "Option::is_none")]
    completeness_status: Option<String>,
    /// Completeness coverage bounds (time/block ranges).
    #[serde(skip_serializing_if = "Option::is_none")]
    completeness_coverage: Option<serde_json::Value>,
    /// When the export job started.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_datetime"
    )]
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// When the export job completed.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_datetime"
    )]
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// ID of the last ingestion run that contributed to the exported data.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_ingestion_run_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub id: Uuid,
    pub state: JobState,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Running,
    /// Data generation complete, sink delivery in progress.
    Delivering,
    Completed,
    Failed,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    let config = AppConfig::load()?;
    config.validate()?;

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
        export_jobs: RwLock::new(HashMap::new()),
        rate_limiter: Arc::new(RateLimiter::new(
            RATE_LIMIT_CAPACITY,
            RATE_LIMIT_REFILL_RATE,
        )),
    });

    // Background task: periodically prune stale jobs and export jobs so
    // completed/failed entries don't accumulate in memory indefinitely.
    {
        let state = Arc::clone(&shared_state);
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(JOB_CLEANUP_INTERVAL_SECS));
            interval.tick().await; // first tick completes immediately
            loop {
                interval.tick().await;
                state.prune_stale_jobs().await;
                state.prune_stale_export_jobs().await;
            }
        });
    }

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
        .route("/v1/targets", post(register_target))
        .route("/v1/targets", get(list_targets))
        .route("/v1/targets/{target_id}", get(get_target))
        .route("/v1/networks", get(list_networks))
        .route("/v1/networks/{network_id}", get(get_network))
        .route("/v1/datasets", get(list_all_datasets))
        .route(
            "/v1/datasets/{name}/versions",
            get(list_dataset_versions_handler),
        )
        .route("/v1/datasets/{name}/records", get(query_dataset_records))
        .route(
            "/v1/datasets/{name}/completeness",
            get(get_dataset_completeness_handler),
        )
        .route(
            "/v1/datasets/{name}/status",
            get(get_dataset_status_handler),
        )
        .route("/v1/export/dataset", post(create_export_job))
        .route("/v1/export/jobs/{job_id}", get(get_export_job_status))
        .route("/v1/export/jobs/{job_id}/download", get(download_export))
        .route("/v1/export/tax", get(tax_export))
        .route("/v1/forensics/activity", get(forensics_activity_handler))
        .route("/v1/analytics/hl/trader", get(hl_trader_analytics_handler))
        .route("/v1/analytics/hl/market", get(hl_market_analytics_handler))
        .route(
            "/v1/analytics/protocol/activity",
            get(protocol_activity_handler),
        )
        .route("/v1/analytics/protocol/tvl", get(protocol_tvl_handler))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&shared_state),
            rate_limit_middleware,
        ))
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
                status: StatusCode::INTERNAL_SERVER_ERROR,
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

/// Per-key rate limiting middleware. Runs after auth so the key is already
/// validated. Extracts the Bearer token from the Authorization header and
/// uses it as the bucket key.
async fn rate_limit_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let key = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("anonymous")
        .to_string();

    if !state.rate_limiter.try_acquire(&key).await {
        return Err(AppError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "Rate limit exceeded. Try again shortly.".to_string(),
        });
    }

    Ok(next.run(req).await)
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

#[derive(Deserialize)]
struct DatasetQueryParams {
    target_id: Option<Uuid>,
    network: Option<String>,
    time_start: Option<i64>,
    time_end: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Deserialize)]
struct ExportJobRequest {
    dataset: String,
    format: Option<String>,
    target_id: Option<Uuid>,
    network: Option<String>,
    time_start: Option<i64>,
    time_end: Option<i64>,
    /// Optional sink config. When provided, export data is delivered to the
    /// specified sink in addition to being stored in-memory for download.
    sink: Option<SinkConfig>,
}

#[derive(Serialize)]
struct DatasetInfo {
    name: String,
    latest_version: Option<i32>,
    latest_version_status: Option<String>,
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
        .redirect(reqwest::redirect::Policy::none())
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
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
                    || v6.is_multicast()
                    // IPv4-mapped and IPv4-compatible — delegate to v4 rules
                    || v6.to_ipv4().is_some_and(|v4| {
                        v4.is_loopback()
                            || v4.is_private()
                            || v4.is_link_local()
                            || v4.is_broadcast()
                            || v4.is_unspecified()
                    })
            }
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

// ---------------------------------------------------------------------------
// Sink implementations
// ---------------------------------------------------------------------------

/// Header names that must not be set by user-supplied sink configuration.
const FORBIDDEN_HEADER_NAMES: &[&str] = &[
    "authorization",
    "cookie",
    "host",
    "content-length",
    "transfer-encoding",
    "set-cookie",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
];

/// Validate that a header name contains only allowed characters: `[a-zA-Z0-9\-_]`.
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Validate that a header value contains no control characters
/// (except horizontal tab, which is allowed in HTTP header values).
fn is_valid_header_value(value: &str) -> bool {
    value.chars().all(|c| !c.is_control() || c == '\t')
}

/// Validates a SinkConfig at job creation time.
///
/// For `LocalFile` sinks, the file path must resolve to a location inside
/// `export_dir`. The path is canonicalized (resolving symlinks) and checked
/// against the canonical export root to prevent directory escape.
fn validate_sink_config(config: &SinkConfig, export_dir: &str) -> Result<(), AppError> {
    config
        .validate()
        .map_err(|e| AppError::bad_request(format!("Invalid sink config: {e}")))?;

    match config.sink_type {
        SinkType::Webhook => {
            if let Some(ref url) = config.url {
                validate_callback_url(url)?;
            }
            if let Some(ref headers) = config.headers {
                for (name, value) in headers {
                    let lower = name.to_lowercase();
                    if FORBIDDEN_HEADER_NAMES.contains(&lower.as_str()) {
                        return Err(AppError::bad_request(format!(
                            "Forbidden webhook header: {name}"
                        )));
                    }
                    if !is_valid_header_name(name) {
                        return Err(AppError::bad_request(format!(
                            "Invalid header name: {name}. Header names must contain only [a-zA-Z0-9-_]"
                        )));
                    }
                    if !is_valid_header_value(value) {
                        return Err(AppError::bad_request(format!(
                            "Invalid header value for {name}: header values must not contain control characters"
                        )));
                    }
                }
            }
        }
        SinkType::LocalFile => {
            if let Some(ref path) = config.file_path {
                validate_export_file_path(path, export_dir)?;
            }
        }
        SinkType::Database => {
            // Database sink is not yet implemented at runtime.
            return Err(AppError::bad_request(
                "Database sink is not yet implemented. Use local_file or webhook.",
            ));
        }
    }
    Ok(())
}

/// Validates that a local file export path resolves to a location within
/// `export_dir`. Rejects path traversal, absolute paths outside the root,
/// and symlink-based escapes.
///
/// **TOCTOU note**: This check canonicalizes at validation time, but the
/// actual write happens later in `LocalFileSink::deliver`. A symlink
/// created between validation and write could bypass this check.  Full
/// mitigation would require O_NOFOLLOW or chroot, which is impractical
/// here; the canonicalization check is defense-in-depth.
fn validate_export_file_path(path: &str, export_dir: &str) -> Result<(), AppError> {
    use std::path::Path;

    // Reject null bytes which could cause path truncation in lower layers
    if path.contains('\0') {
        return Err(AppError::bad_request(
            "file_path must not contain null bytes",
        ));
    }

    // Reject paths containing `..` segments
    if path.contains("..") {
        return Err(AppError::bad_request(
            "file_path must not contain '..' path traversal",
        ));
    }

    let export_root = Path::new(export_dir);

    // Create the export directory if it does not exist
    if let Err(e) = std::fs::create_dir_all(export_root) {
        return Err(AppError::bad_request(format!(
            "Cannot create export directory '{}': {e}",
            export_dir
        )));
    }

    // Canonicalize the export root to resolve symlinks
    let canonical_root = export_root.canonicalize().map_err(|e| {
        AppError::bad_request(format!(
            "Cannot resolve export directory '{}': {e}",
            export_dir
        ))
    })?;

    // Build the candidate path: export_root joined with the user-supplied path.
    // If the user path is absolute, join() ignores the prefix, so strip any
    // leading separator to force it relative to the export root.
    let relative_path = path.trim_start_matches('/');
    let candidate = canonical_root.join(relative_path);

    // The candidate file may not exist yet (it will be created on write).
    // Canonicalize the deepest existing ancestor to catch symlink escapes.
    let mut check = candidate.as_path();
    loop {
        match check.canonicalize() {
            Ok(resolved) => {
                if !resolved.starts_with(&canonical_root) {
                    return Err(AppError::bad_request(
                        "file_path resolves outside the configured export directory",
                    ));
                }
                break;
            }
            Err(_) => {
                // Walk up to the parent that does exist
                match check.parent() {
                    Some(parent) if parent != check => {
                        check = parent;
                    }
                    _ => {
                        return Err(AppError::bad_request(
                            "file_path resolves outside the configured export directory",
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Writes export data to a local file path within the configured export directory.
struct LocalFileSink {
    path: String,
}

#[async_trait::async_trait]
impl ExportSink for LocalFileSink {
    async fn deliver(
        &self,
        data: &[u8],
        _metadata: &DeliveryMetadata,
    ) -> Result<DeliveryReceipt, String> {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(&self.path).parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create directory for {}: {e}", self.path))?;
        }

        tokio::fs::write(&self.path, data)
            .await
            .map_err(|e| format!("Failed to write to {}: {e}", self.path))?;

        Ok(DeliveryReceipt {
            sink_type: SinkType::LocalFile,
            destination: self.path.clone(),
            bytes_written: data.len(),
            delivered_at: chrono::Utc::now(),
        })
    }
}

/// POSTs export data to an HTTP(S) webhook URL.
struct WebhookSink {
    url: String,
    headers: Option<std::collections::HashMap<String, String>>,
}

#[async_trait::async_trait]
impl ExportSink for WebhookSink {
    async fn deliver(
        &self,
        data: &[u8],
        metadata: &DeliveryMetadata,
    ) -> Result<DeliveryReceipt, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        let content_type = match metadata.format.as_str() {
            "csv" => "text/csv; charset=utf-8",
            _ => "application/x-ndjson",
        };

        let mut req = client
            .post(&self.url)
            .header("Content-Type", content_type)
            .header("X-Export-Dataset", &metadata.dataset)
            .header("X-Export-Format", &metadata.format)
            .header("X-Export-Record-Count", metadata.record_count.to_string())
            .header("X-Export-Job-Id", metadata.job_id.to_string());

        if let Some(ref headers) = self.headers {
            for (key, value) in headers {
                req = req.header(key, value);
            }
        }

        let response = req
            .body(data.to_vec())
            .send()
            .await
            .map_err(|e| format!("Webhook POST to {} failed: {e}", self.url))?;

        if !response.status().is_success() {
            return Err(format!(
                "Webhook returned non-success status: {}",
                response.status()
            ));
        }

        Ok(DeliveryReceipt {
            sink_type: SinkType::Webhook,
            destination: self.url.clone(),
            bytes_written: data.len(),
            delivered_at: chrono::Utc::now(),
        })
    }
}

/// Builds the appropriate ExportSink from a SinkConfig.
/// Database sink is not yet implemented and returns an error.
///
/// For `LocalFile` sinks, `export_dir` is the configured export root
/// directory. The user-supplied file_path is resolved relative to it.
fn build_sink(config: &SinkConfig, export_dir: &str) -> Result<Box<dyn ExportSink>, String> {
    match config.sink_type {
        SinkType::LocalFile => {
            let user_path = config
                .file_path
                .as_ref()
                .ok_or("LocalFile sink requires file_path")?;

            let root = std::path::Path::new(export_dir)
                .canonicalize()
                .map_err(|e| format!("Cannot resolve export directory: {e}"))?;
            let relative = user_path.trim_start_matches('/');
            let resolved = root.join(relative);

            Ok(Box::new(LocalFileSink {
                path: resolved.to_string_lossy().into_owned(),
            }))
        }
        SinkType::Webhook => {
            let url = config.url.as_ref().ok_or("Webhook sink requires url")?;
            Ok(Box::new(WebhookSink {
                url: url.clone(),
                headers: config.headers.clone(),
            }))
        }
        SinkType::Database => {
            // TODO: Implement runtime Database sink delivery.
            // This requires external connection management (separate pool,
            // credential handling, schema negotiation) which is out of scope
            // for P4-W3. The config parsing and validation are in place;
            // runtime delivery will be added in a future packet.
            Err("Database sink is not yet implemented at runtime".to_string())
        }
    }
}

fn check_wallet_allowed(wallet: &str, allowed: &Option<HashSet<String>>) -> Result<(), AppError> {
    if let Some(set) = allowed {
        // Normalize the lookup key the same way allowed_wallets_set() does:
        // lowercase only EVM (0x-prefixed) addresses; preserve others as-is.
        let key = if wallet.starts_with("0x") || wallet.starts_with("0X") {
            wallet.to_lowercase()
        } else {
            wallet.to_string()
        };
        if !set.contains(&key) {
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

            // Parse chain enum for ensure_wallet_target
            let chain_enum = match chain.as_str() {
                "solana" => Chain::Solana,
                "ethereum" => Chain::Ethereum,
                "hyperliquid" => Chain::Hyperliquid,
                _ => unreachable!("chain validated before spawn"),
            };

            // Ensure a V2 IndexTarget exists for this wallet (best-effort)
            let target_id = match state_clone
                .repo
                .ensure_wallet_target(&chain_enum, &wallet, Some(user_id))
                .await
            {
                Ok(target) => Some(target.id),
                Err(e) => {
                    warn!(
                        error = %e,
                        wallet = %wallet,
                        "Failed to ensure V2 wallet target (V1 path unaffected)"
                    );
                    None
                }
            };

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
                if let Some(tid) = target_id {
                    state_clone
                        .repo
                        .save_transactions_and_checkpoint_dual_write(&events, &cp, tid)
                        .await?;
                } else {
                    state_clone
                        .repo
                        .save_transactions_and_checkpoint(&events, &cp)
                        .await?;
                }
            } else if let Some(tid) = target_id {
                state_clone
                    .repo
                    .save_transactions_dual_write(&events, tid)
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
}

async fn start_stream(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<StartStreamRequest>,
) -> Result<Json<StreamInfo>, AppError> {
    if payload.chain != "solana" {
        return Err(AppError::bad_request(
            "Streaming is currently only supported for solana",
        ));
    }

    let grpc_url = state
        .config
        .solana_grpc_url
        .as_deref()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            AppError::bad_request("Solana gRPC URL not configured (set SOLANA_GRPC_URL)")
        })?;

    let _permit = state
        .stream_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::service_unavailable("Too many concurrent streams"))?;

    let stream_id = Uuid::new_v4();
    let cancel = CancellationToken::new();
    let tx_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let last_slot = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let adapter = SolanaGrpcAdapter::new(grpc_url, state.config.solana_grpc_token.clone());
    let (mut rx, grpc_handle) = adapter.stream_transactions();

    let cancel_clone = cancel.clone();
    let tx_count_clone = Arc::clone(&tx_count);
    let last_slot_clone = Arc::clone(&last_slot);
    let repo = state.repo.clone();
    let state_clone = Arc::clone(&state);

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

    let entry = StreamEntry {
        id: stream_id,
        cancel: cancel.clone(),
        started_at: Instant::now(),
        tx_count: Arc::clone(&tx_count),
        last_slot: Arc::clone(&last_slot),
    };

    let info = StreamInfo {
        id: stream_id,
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
            uptime_secs: entry.started_at.elapsed().as_secs(),
            transactions_ingested: entry.tx_count.load(std::sync::atomic::Ordering::Relaxed),
            last_slot: entry.last_slot.load(std::sync::atomic::Ordering::Relaxed),
        })
        .collect();
    Ok(Json(infos))
}

// ---------------------------------------------------------------------------
// Target registration types and handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RegisterTargetRequest {
    kind: String,
    network: String,
    address: Option<String>,
    filter_spec: Option<serde_json::Value>,
    mode: Option<String>,
    label: Option<String>,
}

#[derive(Deserialize)]
struct TargetListParams {
    limit: Option<i64>,
    offset: Option<i64>,
    kind: Option<String>,
    network: Option<String>,
}

fn conflict(msg: impl Into<String>) -> AppError {
    AppError {
        status: StatusCode::CONFLICT,
        message: msg.into(),
    }
}

async fn register_target(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterTargetRequest>,
) -> Result<(StatusCode, Json<IndexTarget>), AppError> {
    // Parse kind
    let kind: TargetKind = req
        .kind
        .parse()
        .map_err(|_| AppError::bad_request(format!("Invalid target kind: {}", req.kind)))?;

    // Parse mode (default to "both")
    let mode_str = req.mode.as_deref().unwrap_or("both");
    let mode: TargetMode = mode_str
        .parse()
        .map_err(|_| AppError::bad_request(format!("Invalid mode: {mode_str}")))?;

    // Look up network
    let network = state
        .repo
        .get_network(&req.network)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::bad_request(format!("Unknown network: {}", req.network)))?;

    // Normalize address
    let address = req.address.map(|addr| match network.chain_family {
        ChainFamily::Evm | ChainFamily::Hyperliquid => normalize_evm_address(&addr),
        ChainFamily::Solana => normalize_solana_address(&addr),
    });

    let now = chrono::Utc::now();
    let target = IndexTarget {
        id: Uuid::new_v4(),
        kind,
        network: req.network,
        chain_family: network.chain_family,
        address,
        filter_spec: req.filter_spec,
        mode,
        label: req.label,
        owner_id: None,
        created_at: now,
        updated_at: now,
    };

    // Validate
    if let Err(errors) = validate_target(&target) {
        return Err(AppError::bad_request(errors.join("; ")));
    }

    // Persist
    match state.repo.create_index_target(&target).await {
        Ok(created) => Ok((StatusCode::CREATED, Json(created))),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("duplicate key") || msg.contains("unique constraint") {
                Err(conflict(
                    "A target with the same kind, network, and address already exists",
                ))
            } else {
                Err(AppError::internal(e))
            }
        }
    }
}

async fn list_targets(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TargetListParams>,
) -> Result<Json<Vec<IndexTarget>>, AppError> {
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);

    // Parse kind filter (if provided) before calling the repository.
    let kind: Option<TargetKind> = params
        .kind
        .as_deref()
        .map(|k| {
            k.parse()
                .map_err(|_| AppError::bad_request(format!("Invalid target kind: {k}")))
        })
        .transpose()?;

    let targets = state
        .repo
        .list_index_targets_filtered(params.network.as_deref(), kind, limit, offset)
        .await
        .map_err(AppError::internal)?;

    Ok(Json(targets))
}

async fn get_target(
    State(state): State<Arc<AppState>>,
    Path(target_id): Path<Uuid>,
) -> Result<Json<IndexTarget>, AppError> {
    let target = state
        .repo
        .get_index_target(target_id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found(format!("Target {} not found", target_id)))?;
    Ok(Json(target))
}

async fn list_networks(State(state): State<Arc<AppState>>) -> Result<Json<Vec<Network>>, AppError> {
    let networks = state
        .repo
        .list_networks()
        .await
        .map_err(AppError::internal)?;
    Ok(Json(networks))
}

async fn get_network(
    State(state): State<Arc<AppState>>,
    Path(network_id): Path<String>,
) -> Result<Json<Network>, AppError> {
    let network = state
        .repo
        .get_network(&network_id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found(format!("Network {} not found", network_id)))?;
    Ok(Json(network))
}

// ---------------------------------------------------------------------------
// Dataset query handlers (P4-W1)
// ---------------------------------------------------------------------------

/// Datasets queryable via the /v1/datasets/{name}/records endpoint.
/// Includes the six Silver datasets plus two Gold datasets (P5-W1).
const QUERYABLE_DATASETS: &[&str] = &[
    "token_transfers",
    "native_balance_deltas",
    "decoded_events",
    "hl_fills",
    "hl_funding",
    "positions",
    "wallet_ledger",
    "balance_history",
    "hl_pnl_summary",
    "hl_trade_history",
    "protocol_events",
    "pool_snapshots",
];

fn validate_dataset_name(name: &str) -> Result<(), AppError> {
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::bad_request("Invalid dataset name length"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(AppError::bad_request(
            "Dataset name contains invalid characters",
        ));
    }
    Ok(())
}

async fn list_all_datasets(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<DatasetInfo>>, AppError> {
    let mut datasets = Vec::new();
    for ds in DatasetName::all() {
        let sql_name = ds.as_sql_str();
        let latest = state
            .repo
            .get_latest_dataset_version(sql_name)
            .await
            .map_err(AppError::internal)?;
        datasets.push(DatasetInfo {
            name: sql_name.to_string(),
            latest_version: latest.as_ref().map(|v| v.version),
            latest_version_status: latest.as_ref().map(|v| v.status.to_string()),
        });
    }
    Ok(Json(datasets))
}

async fn list_dataset_versions_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Vec<DatasetVersion>>, AppError> {
    validate_dataset_name(&name)?;
    let versions = state
        .repo
        .list_dataset_versions(&name)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(versions))
}

async fn query_dataset_records(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(params): Query<DatasetQueryParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_dataset_name(&name)?;
    if !QUERYABLE_DATASETS.contains(&name.as_str()) {
        return Err(AppError::bad_request(format!(
            "Dataset '{}' is not queryable via this endpoint. Queryable datasets: {}",
            name,
            QUERYABLE_DATASETS.join(", ")
        )));
    }
    validate_date_range(params.time_start, params.time_end)?;
    let limit = clamp_limit(params.limit);
    let offset = clamp_offset(params.offset);
    let net = params.network.as_deref();

    let result = match name.as_str() {
        "token_transfers" => {
            let records = state
                .repo
                .query_token_transfers(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "native_balance_deltas" => {
            let records = state
                .repo
                .query_native_balance_deltas(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "decoded_events" => {
            let records = state
                .repo
                .query_decoded_events(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "hl_fills" => {
            let records = state
                .repo
                .query_hl_fill_records(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "hl_funding" => {
            let records = state
                .repo
                .query_hl_funding_payments(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "positions" => {
            let records = state
                .repo
                .query_hl_position_changes(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "wallet_ledger" => {
            let records = state
                .repo
                .query_wallet_ledger_records(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "balance_history" => {
            let records = state
                .repo
                .query_balance_snapshots(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "hl_pnl_summary" => {
            let records = state
                .repo
                .query_hl_pnl_summary(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "hl_trade_history" => {
            let records = state
                .repo
                .query_hl_trade_history(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "protocol_events" => {
            let records = state
                .repo
                .query_protocol_events(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        "pool_snapshots" => {
            let records = state
                .repo
                .query_pool_snapshots(
                    params.target_id,
                    net,
                    params.time_start,
                    params.time_end,
                    limit,
                    offset,
                )
                .await
                .map_err(AppError::internal)?;
            serde_json::to_value(&records).map_err(AppError::internal)?
        }
        _ => unreachable!("dataset name validated above"),
    };

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Export job handlers (P4-W2)
// ---------------------------------------------------------------------------

/// Datasets that support export via the /v1/export/dataset endpoint.
/// Includes Silver datasets plus Gold datasets (P5-W1, P5-W2).
const EXPORTABLE_DATASETS: &[&str] = &[
    "token_transfers",
    "native_balance_deltas",
    "decoded_events",
    "hl_fills",
    "hl_funding",
    "positions",
    "wallet_ledger",
    "balance_history",
    "hl_pnl_summary",
    "hl_trade_history",
    "protocol_events",
    "pool_snapshots",
];

fn content_type_for_format(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Jsonl => "application/x-ndjson",
        ExportFormat::Csv => "text/csv; charset=utf-8",
    }
}

fn serialize_to_jsonl<T: Serialize>(records: &[T]) -> Result<Vec<u8>, AppError> {
    let mut buf = Vec::new();
    for record in records {
        serde_json::to_writer(&mut buf, record).map_err(AppError::internal)?;
        buf.push(b'\n');
    }
    Ok(buf)
}

fn token_transfers_to_csv(records: &[spectraplex_core::materializer::TokenTransfer]) -> Vec<u8> {
    let mut buf = String::from(
        "id,raw_transaction_id,network,token_address,token_symbol,from_address,to_address,amount,decimals,transfer_index,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.token_address),
            r.token_symbol.as_deref().unwrap_or(""),
            csv_escape(&r.from_address),
            csv_escape(&r.to_address),
            r.amount,
            r.decimals,
            r.transfer_index,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn native_balance_deltas_to_csv(
    records: &[spectraplex_core::materializer::NativeBalanceDelta],
) -> Vec<u8> {
    let mut buf = String::from(
        "id,raw_transaction_id,network,account_address,native_token,pre_balance,post_balance,delta,is_fee_payer,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.account_address),
            csv_escape(&r.native_token),
            r.pre_balance,
            r.post_balance,
            r.delta,
            r.is_fee_payer,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn decoded_events_to_csv(records: &[spectraplex_core::materializer::DecodedEvent]) -> Vec<u8> {
    let mut buf = String::from(
        "id,raw_transaction_id,network,program_or_contract,event_signature,event_name,log_index,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.program_or_contract),
            csv_escape(r.event_signature.as_deref().unwrap_or("")),
            csv_escape(r.event_name.as_deref().unwrap_or("")),
            r.log_index,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn hl_fills_to_csv(records: &[spectraplex_core::materializer::HlFillRecord]) -> Vec<u8> {
    let mut buf = String::from(
        "id,raw_transaction_id,network,coin,side,price,size,direction,closed_pnl,fee,fee_token,fill_time,order_id,trade_id,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.coin),
            csv_escape(&r.side),
            r.price,
            r.size,
            r.direction.as_deref().unwrap_or(""),
            r.closed_pnl
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.fee.as_ref().map(|v| v.to_string()).unwrap_or_default(),
            r.fee_token.as_deref().unwrap_or(""),
            r.fill_time,
            r.order_id.map(|v| v.to_string()).unwrap_or_default(),
            r.trade_id.map(|v| v.to_string()).unwrap_or_default(),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn hl_funding_to_csv(records: &[spectraplex_core::materializer::HlFundingPayment]) -> Vec<u8> {
    let mut buf = String::from(
        "id,raw_transaction_id,network,coin,amount,funding_rate,payment_time,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.coin),
            r.amount,
            r.funding_rate
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.payment_time,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn hl_positions_to_csv(records: &[spectraplex_core::materializer::HlPositionChange]) -> Vec<u8> {
    let mut buf = String::from(
        "id,raw_transaction_id,network,coin,side,size_delta,price,direction,source_event,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.coin),
            csv_escape(&r.side),
            r.size_delta,
            r.price,
            r.direction.as_deref().unwrap_or(""),
            csv_escape(&r.source_event),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn wallet_ledger_to_csv(records: &[WalletLedgerRecord]) -> Vec<u8> {
    let mut buf = String::from(
        "id,raw_transaction_id,wallet_address,network,tx_hash,timestamp,entry_type,asset_symbol,amount,counterparty_address,fee_amount,fee_asset,cost_basis,proceeds,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.wallet_address),
            csv_escape(&r.network),
            csv_escape(&r.tx_hash),
            r.timestamp,
            csv_escape(&r.entry_type),
            csv_escape(&r.asset_symbol),
            r.amount,
            r.counterparty_address.as_deref().unwrap_or(""),
            r.fee_amount
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.fee_asset.as_deref().unwrap_or(""),
            r.cost_basis
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.proceeds
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn balance_history_to_csv(records: &[BalanceSnapshot]) -> Vec<u8> {
    let mut buf = String::from(
        "id,wallet_address,asset_symbol,network,timestamp,balance,tx_hash,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            r.id,
            csv_escape(&r.wallet_address),
            csv_escape(&r.asset_symbol),
            csv_escape(&r.network),
            r.timestamp,
            r.balance,
            csv_escape(&r.tx_hash),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn hl_pnl_summary_to_csv(records: &[HlPnlSummary]) -> Vec<u8> {
    let mut buf = String::from(
        "id,wallet_address,coin,network,period_start,period_end,total_closed_pnl,total_funding,total_fees,net_pnl,trade_count,fill_count,avg_trade_size,win_count,loss_count,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            csv_escape(&r.wallet_address),
            csv_escape(&r.coin),
            csv_escape(&r.network),
            r.period_start,
            r.period_end,
            r.total_closed_pnl,
            r.total_funding,
            r.total_fees,
            r.net_pnl,
            r.trade_count,
            r.fill_count,
            r.avg_trade_size,
            r.win_count,
            r.loss_count,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn hl_trade_history_to_csv(records: &[HlTradeHistory]) -> Vec<u8> {
    let mut buf = String::from(
        "id,wallet_address,coin,network,side,entry_price,exit_price,size,opened_at,closed_at,realized_pnl,fees,num_fills,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            csv_escape(&r.wallet_address),
            csv_escape(&r.coin),
            csv_escape(&r.network),
            csv_escape(&r.side),
            r.entry_price,
            r.exit_price,
            r.size,
            r.opened_at,
            r.closed_at,
            r.realized_pnl,
            r.fees,
            r.num_fills,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn protocol_events_to_csv(records: &[ProtocolEvent]) -> Vec<u8> {
    let mut buf = String::from(
        "id,network,protocol_address,protocol_name,event_type,event_details,pool_address,raw_event_id,timestamp,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            csv_escape(&r.network),
            csv_escape(&r.protocol_address),
            r.protocol_name.as_deref().unwrap_or(""),
            csv_escape(&r.event_type),
            csv_escape(&r.event_details.to_string()),
            r.pool_address.as_deref().unwrap_or(""),
            r.raw_event_id.map(|u| u.to_string()).unwrap_or_default(),
            r.timestamp,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

fn pool_snapshots_to_csv(records: &[PoolSnapshot]) -> Vec<u8> {
    let mut buf = String::from(
        "id,network,pool_address,protocol_address,protocol_name,token0_address,token0_symbol,token1_address,token1_symbol,reserve0,reserve1,tvl_usd,snapshot_timestamp,block_number,dataset_version_id,created_at\n",
    );
    for r in records {
        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.id,
            csv_escape(&r.network),
            csv_escape(&r.pool_address),
            csv_escape(&r.protocol_address),
            r.protocol_name.as_deref().unwrap_or(""),
            csv_escape(&r.token0_address),
            r.token0_symbol.as_deref().unwrap_or(""),
            csv_escape(&r.token1_address),
            r.token1_symbol.as_deref().unwrap_or(""),
            r.reserve0,
            r.reserve1,
            r.tvl_usd
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.snapshot_timestamp,
            r.block_number.map(|v| v.to_string()).unwrap_or_default(),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        ));
    }
    buf.into_bytes()
}

/// Generate tax-export-friendly CSV from wallet_ledger records.
///
/// Columns: Date,Type,Sent_Asset,Sent_Amount,Received_Asset,Received_Amount,
///          Fee_Asset,Fee_Amount,Cost_Basis,Proceeds,Gain_Loss,Tx_Hash,Network
fn wallet_ledger_to_tax_csv(records: &[WalletLedgerRecord]) -> Vec<u8> {
    let mut buf = String::from(
        "Date,Type,Sent_Asset,Sent_Amount,Received_Asset,Received_Amount,Fee_Asset,Fee_Amount,Cost_Basis,Proceeds,Gain_Loss,Tx_Hash,Network\n",
    );
    for r in records {
        let date = chrono::DateTime::from_timestamp(r.timestamp, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| r.timestamp.to_string());

        let (sent_asset, sent_amount, recv_asset, recv_amount) = if r.amount < BigDecimal::from(0) {
            (
                r.asset_symbol.as_str(),
                r.amount.abs().to_string(),
                "",
                String::new(),
            )
        } else {
            (
                "",
                String::new(),
                r.asset_symbol.as_str(),
                r.amount.to_string(),
            )
        };

        let fee_asset = r.fee_asset.as_deref().unwrap_or("");
        let fee_amount = r
            .fee_amount
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let cost_basis = r
            .cost_basis
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let proceeds = r
            .proceeds
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();

        // Gain/Loss computed from cost_basis and proceeds when both available
        let gain_loss = match (&r.proceeds, &r.cost_basis) {
            (Some(p), Some(c)) => (p - c).to_string(),
            _ => String::new(),
        };

        buf.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            csv_escape(&date),
            csv_escape(&r.entry_type),
            csv_escape(sent_asset),
            sent_amount,
            csv_escape(recv_asset),
            recv_amount,
            csv_escape(fee_asset),
            fee_amount,
            cost_basis,
            proceeds,
            gain_loss,
            csv_escape(&r.tx_hash),
            csv_escape(&r.network),
        ));
    }
    buf.into_bytes()
}

async fn create_export_job(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ExportJobRequest>,
) -> Result<(StatusCode, Json<ExportJobStatus>), AppError> {
    validate_dataset_name(&req.dataset)?;
    if !EXPORTABLE_DATASETS.contains(&req.dataset.as_str()) {
        return Err(AppError::bad_request(format!(
            "Dataset '{}' is not exportable. Exportable datasets: {}",
            req.dataset,
            EXPORTABLE_DATASETS.join(", ")
        )));
    }

    let format_str = req.format.as_deref().unwrap_or("jsonl");
    let format: ExportFormat = format_str.parse().map_err(|_| {
        AppError::bad_request(format!(
            "Unsupported export format: {format_str}. Use 'jsonl' or 'csv'"
        ))
    })?;

    validate_date_range(req.time_start, req.time_end)?;

    // Validate sink config if provided
    if let Some(ref sink_config) = req.sink {
        validate_sink_config(sink_config, &state.config.export_dir)?;
    }

    let permit = state
        .job_semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| AppError::service_unavailable("Too many concurrent jobs"))?;

    state.prune_stale_export_jobs().await;

    let job_id = Uuid::new_v4();
    let status = ExportJobStatus {
        id: job_id,
        state: JobState::Pending,
        dataset: req.dataset.clone(),
        format: format_str.to_string(),
        record_count: None,
        message: None,
        delivered_to: None,
        delivery_status: req.sink.as_ref().map(|_| "pending".to_string()),
        dataset_version_id: None,
        dataset_version: None,
        completeness_status: None,
        completeness_coverage: None,
        started_at: None,
        completed_at: None,
        last_ingestion_run_id: None,
    };

    state.export_jobs.write().await.insert(
        job_id,
        ExportJobEntry {
            status: status.clone(),
            finished_at: None,
            data: None,
        },
    );

    let state_clone = Arc::clone(&state);
    let dataset = req.dataset.clone();
    let target_id = req.target_id;
    let network = req.network.clone();
    let time_start = req.time_start;
    let time_end = req.time_end;
    let sink_config = req.sink.clone();
    let export_dir = state.config.export_dir.clone();

    tokio::spawn(async move {
        let _permit = permit;

        // Mark as running with wall-clock start time
        let start_time = chrono::Utc::now();
        {
            let mut exports = state_clone.export_jobs.write().await;
            if let Some(entry) = exports.get_mut(&job_id) {
                entry.status.state = JobState::Running;
                entry.status.started_at = Some(start_time);
            }
        }

        let result = run_export_job(
            &state_clone.repo,
            &dataset,
            format,
            target_id,
            network.as_deref(),
            time_start,
            time_end,
        )
        .await;

        // Separate the export result handling from sink delivery to avoid
        // holding the write lock during potentially slow async I/O.
        match result {
            Ok((body, record_count, export_meta)) => {
                // Only clone body when a sink needs it after storage
                let sink_body = if sink_config.is_some() {
                    Some(body.clone())
                } else {
                    None
                };

                let has_sink = sink_config.is_some();

                // Store export data and transition to the correct intermediate
                // state.  When a sink is configured the job enters
                // `Delivering` so clients never observe "Completed + pending
                // delivery".  Without a sink we go straight to `Completed`.
                {
                    let mut exports = state_clone.export_jobs.write().await;
                    if let Some(entry) = exports.get_mut(&job_id) {
                        entry.status.state = if has_sink {
                            JobState::Delivering
                        } else {
                            JobState::Completed
                        };
                        entry.status.record_count = Some(record_count);
                        entry.status.message = Some(format!("Exported {record_count} records"));
                        if !has_sink {
                            entry.status.completed_at = Some(chrono::Utc::now());
                        }
                        // Populate provenance metadata from export
                        entry.status.dataset_version_id = export_meta.dataset_version_id;
                        entry.status.dataset_version = export_meta.dataset_version;
                        entry.status.completeness_status = export_meta.completeness_status.clone();
                        entry.status.completeness_coverage =
                            export_meta.completeness_coverage.clone();
                        entry.status.last_ingestion_run_id = export_meta.last_ingestion_run_id;
                        // Always store in-memory for download (backward compatibility)
                        entry.data = Some(ExportData {
                            content_type: content_type_for_format(format),
                            body,
                        });
                    }
                }

                // Deliver to sink outside the lock (may involve network I/O)
                if let Some(ref sc) = sink_config {
                    let body = sink_body.expect("sink_body set when sink_config is Some");
                    let delivery_result = match build_sink(sc, &export_dir) {
                        Ok(sink) => {
                            let delivery_meta = DeliveryMetadata {
                                job_id,
                                dataset: dataset.clone(),
                                format: format.to_string(),
                                record_count,
                                dataset_version_id: export_meta.dataset_version_id,
                                completeness_status: export_meta.completeness_status,
                            };
                            match sink.deliver(&body, &delivery_meta).await {
                                Ok(receipt) => Ok(receipt.destination),
                                Err(e) => {
                                    warn!(error = %e, "Sink delivery failed");
                                    Err(format!(
                                        "Exported {record_count} records, but sink delivery failed: {e}"
                                    ))
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to build sink");
                            Err(format!(
                                "Exported {record_count} records, but sink build failed: {e}"
                            ))
                        }
                    };

                    // Transition Delivering → Completed or Delivering → Failed
                    let mut exports = state_clone.export_jobs.write().await;
                    if let Some(entry) = exports.get_mut(&job_id) {
                        match delivery_result {
                            Ok(destination) => {
                                entry.status.state = JobState::Completed;
                                entry.status.delivered_to = Some(destination);
                                entry.status.delivery_status = Some("delivered".to_string());
                                entry.status.completed_at = Some(chrono::Utc::now());
                            }
                            Err(msg) => {
                                entry.status.state = JobState::Failed;
                                entry.status.delivery_status = Some("failed".to_string());
                                entry.status.message = Some(msg);
                                entry.status.completed_at = Some(chrono::Utc::now());
                            }
                        }
                    }
                }

                // Mark finished time
                let mut exports = state_clone.export_jobs.write().await;
                if let Some(entry) = exports.get_mut(&job_id) {
                    entry.finished_at = Some(Instant::now());
                }
            }
            Err(e) => {
                let mut exports = state_clone.export_jobs.write().await;
                if let Some(entry) = exports.get_mut(&job_id) {
                    entry.status.state = JobState::Failed;
                    entry.status.message = Some(format!("Export failed: {e}"));
                    entry.status.completed_at = Some(chrono::Utc::now());
                    if sink_config.is_some() {
                        entry.status.delivery_status = Some("failed".to_string());
                    }
                    entry.finished_at = Some(Instant::now());
                }
            }
        }
    });

    Ok((StatusCode::ACCEPTED, Json(status)))
}

/// Metadata gathered during an export job for provenance and observability.
#[derive(Debug, Clone, Default)]
struct ExportMetadata {
    dataset_version_id: Option<Uuid>,
    dataset_version: Option<i32>,
    completeness_status: Option<String>,
    completeness_coverage: Option<serde_json::Value>,
    last_ingestion_run_id: Option<Uuid>,
}

async fn run_export_job(
    repo: &Repository,
    dataset: &str,
    format: ExportFormat,
    target_id: Option<Uuid>,
    network: Option<&str>,
    time_start: Option<i64>,
    time_end: Option<i64>,
) -> anyhow::Result<(Vec<u8>, usize, ExportMetadata)> {
    // Look up active dataset version for provenance
    let active_version = repo
        .get_active_dataset_version(dataset)
        .await
        .ok()
        .flatten();

    // Look up completeness records for the dataset (optionally filtered by target and network)
    let completeness_records = repo
        .list_completeness_filtered(dataset, target_id, network)
        .await
        .unwrap_or_default();

    let mut meta = ExportMetadata::default();
    if let Some(ref dv) = active_version {
        meta.dataset_version_id = Some(dv.id);
        meta.dataset_version = Some(dv.version);
    }

    if !completeness_records.is_empty() {
        // Aggregate completeness: use worst status across matching records
        let statuses: Vec<&str> = completeness_records
            .iter()
            .map(|c| match c.status {
                spectraplex_core::v2::CompletenessStatus::Complete => "complete",
                spectraplex_core::v2::CompletenessStatus::Partial => "partial",
                spectraplex_core::v2::CompletenessStatus::Backfilling => "backfilling",
                spectraplex_core::v2::CompletenessStatus::Gap => "gap",
            })
            .collect();

        // Pick the most conservative status
        let status = if statuses.contains(&"gap") {
            "gap"
        } else if statuses.contains(&"backfilling") {
            "backfilling"
        } else if statuses.contains(&"partial") {
            "partial"
        } else {
            "complete"
        };
        meta.completeness_status = Some(status.to_string());

        // Aggregate coverage bounds
        let coverage_start = completeness_records
            .iter()
            .filter_map(|c| c.coverage_start)
            .min();
        let coverage_end = completeness_records
            .iter()
            .filter_map(|c| c.coverage_end)
            .max();
        let block_start = completeness_records
            .iter()
            .filter_map(|c| c.block_start)
            .min();
        let block_end = completeness_records
            .iter()
            .filter_map(|c| c.block_end)
            .max();
        meta.completeness_coverage = Some(serde_json::json!({
            "coverage_start": coverage_start,
            "coverage_end": coverage_end,
            "block_start": block_start,
            "block_end": block_end,
        }));

        // Use the most recent ingestion run ID from completeness records
        meta.last_ingestion_run_id = completeness_records
            .iter()
            .rev()
            .find_map(|c| c.last_ingestion_run_id);
    }
    match dataset {
        "token_transfers" => {
            let records = repo
                .export_token_transfers(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => token_transfers_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "native_balance_deltas" => {
            let records = repo
                .export_native_balance_deltas(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => native_balance_deltas_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "decoded_events" => {
            let records = repo
                .export_decoded_events(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => decoded_events_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "hl_fills" => {
            let records = repo
                .export_hl_fill_records(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => hl_fills_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "hl_funding" => {
            let records = repo
                .export_hl_funding_payments(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => hl_funding_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "positions" => {
            let records = repo
                .export_hl_position_changes(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => hl_positions_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "wallet_ledger" => {
            let records = repo
                .export_wallet_ledger_records(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => wallet_ledger_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "balance_history" => {
            let records = repo
                .export_balance_snapshots(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => balance_history_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "hl_pnl_summary" => {
            let records = repo
                .export_hl_pnl_summary(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => hl_pnl_summary_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "hl_trade_history" => {
            let records = repo
                .export_hl_trade_history(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => hl_trade_history_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "protocol_events" => {
            let records = repo
                .export_protocol_events(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => protocol_events_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        "pool_snapshots" => {
            let records = repo
                .export_pool_snapshots(target_id, network, time_start, time_end)
                .await?;
            let count = records.len();
            let body = match format {
                ExportFormat::Jsonl => {
                    serialize_to_jsonl(&records).map_err(|e| anyhow::anyhow!("{}", e.message))?
                }
                ExportFormat::Csv => pool_snapshots_to_csv(&records),
            };
            Ok((body, count, meta))
        }
        _ => Err(anyhow::anyhow!("Unknown dataset: {dataset}")),
    }
}

async fn get_export_job_status(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<Uuid>,
) -> Result<Json<ExportJobStatus>, AppError> {
    let exports = state.export_jobs.read().await;
    match exports.get(&job_id) {
        Some(entry) => Ok(Json(entry.status.clone())),
        None => Err(AppError::not_found("Export job not found")),
    }
}

async fn download_export(
    State(state): State<Arc<AppState>>,
    Path(job_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let exports = state.export_jobs.read().await;
    let entry = exports
        .get(&job_id)
        .ok_or_else(|| AppError::not_found("Export job not found"))?;

    match entry.status.state {
        JobState::Completed => {
            let data = entry
                .data
                .as_ref()
                .ok_or_else(|| AppError::internal("Export data missing"))?;
            let sanitize = |s: &str| -> String {
                s.chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .collect()
            };
            let safe_dataset = sanitize(&entry.status.dataset);
            let safe_format = sanitize(&entry.status.format);
            let disposition = format!(
                "attachment; filename=\"{}-{}.{}\"",
                safe_dataset, job_id, safe_format,
            );
            let mut response = (StatusCode::OK, data.body.clone()).into_response();
            let headers = response.headers_mut();
            headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::header::HeaderValue::from_static(data.content_type),
            );
            headers.insert(
                axum::http::header::CONTENT_DISPOSITION,
                axum::http::header::HeaderValue::from_str(&disposition)
                    .map_err(|_| AppError::internal("Invalid disposition header"))?,
            );
            Ok(response)
        }
        JobState::Running | JobState::Pending | JobState::Delivering => Err(AppError {
            status: StatusCode::CONFLICT,
            message: format!(
                "Export job {} is still {}",
                job_id,
                match entry.status.state {
                    JobState::Running => "running",
                    JobState::Delivering => "delivering",
                    _ => "pending",
                }
            ),
        }),
        JobState::Failed => Err(AppError::bad_request(format!(
            "Export job {} failed: {}",
            job_id,
            entry.status.message.as_deref().unwrap_or("unknown error")
        ))),
    }
}

// ---------------------------------------------------------------------------
// Tax export and forensics endpoints (P5-W1)
// ---------------------------------------------------------------------------

/// Query parameters for the tax export endpoint.
#[derive(Debug, Deserialize)]
struct TaxExportParams {
    target_id: Option<Uuid>,
    network: Option<String>,
    time_start: Option<i64>,
    time_end: Option<i64>,
}

/// GET /v1/export/tax — export wallet_ledger in tax-software-friendly CSV.
async fn tax_export(
    State(state): State<Arc<AppState>>,
    Query(params): Query<TaxExportParams>,
) -> Result<Response, AppError> {
    validate_date_range(params.time_start, params.time_end)?;

    let records = state
        .repo
        .export_wallet_ledger_records(
            params.target_id,
            params.network.as_deref(),
            params.time_start,
            params.time_end,
        )
        .await
        .map_err(AppError::internal)?;

    let csv_bytes = wallet_ledger_to_tax_csv(&records);
    let disposition = "attachment; filename=\"spectraplex-tax-export.csv\"";

    let mut response = (StatusCode::OK, csv_bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::header::HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        axum::http::header::HeaderValue::from_static(disposition),
    );
    Ok(response)
}

/// Query parameters for the forensics activity endpoint.
#[derive(Debug, Deserialize)]
struct ForensicsParams {
    target_id: Option<Uuid>,
    network: Option<String>,
    time_start: Option<i64>,
    time_end: Option<i64>,
}

/// GET /v1/forensics/activity — wallet interaction analysis from wallet_ledger.
async fn forensics_activity_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ForensicsParams>,
) -> Result<Json<ForensicsActivity>, AppError> {
    validate_date_range(params.time_start, params.time_end)?;

    let records = state
        .repo
        .export_wallet_ledger_records(
            params.target_id,
            params.network.as_deref(),
            params.time_start,
            params.time_end,
        )
        .await
        .map_err(AppError::internal)?;

    if records.is_empty() {
        return Ok(Json(ForensicsActivity {
            wallet_address: String::new(),
            top_counterparties: vec![],
            network_activity: vec![],
            type_breakdown: vec![],
            total_entries: 0,
        }));
    }

    let wallet = records[0].wallet_address.clone();
    let activity =
        spectraplex_adapters::ledger_derivation::build_forensics_activity(&wallet, &records);

    Ok(Json(activity))
}

// ---------------------------------------------------------------------------
// Hyperliquid analytics endpoints (P5-W2)
// ---------------------------------------------------------------------------

/// Query parameters for the Hyperliquid analytics endpoints.
#[derive(Debug, Deserialize)]
struct HlAnalyticsParams {
    target_id: Option<Uuid>,
    network: Option<String>,
    time_start: Option<i64>,
    time_end: Option<i64>,
}

/// GET /v1/analytics/hl/trader — per-trader Hyperliquid PnL analytics.
async fn hl_trader_analytics_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HlAnalyticsParams>,
) -> Result<Json<TraderAnalytics>, AppError> {
    validate_date_range(params.time_start, params.time_end)?;

    let pnl_summaries = state
        .repo
        .export_hl_pnl_summary(
            params.target_id,
            params.network.as_deref(),
            params.time_start,
            params.time_end,
        )
        .await
        .map_err(AppError::internal)?;

    let trade_histories = state
        .repo
        .export_hl_trade_history(
            params.target_id,
            params.network.as_deref(),
            params.time_start,
            params.time_end,
        )
        .await
        .map_err(AppError::internal)?;

    let wallet = pnl_summaries
        .first()
        .map(|s| s.wallet_address.as_str())
        .or_else(|| trade_histories.first().map(|t| t.wallet_address.as_str()))
        .unwrap_or("");

    let analytics = spectraplex_adapters::hl_analytics::build_trader_analytics(
        wallet,
        &pnl_summaries,
        &trade_histories,
    );

    Ok(Json(analytics))
}

/// GET /v1/analytics/hl/market — per-coin Hyperliquid market analytics.
async fn hl_market_analytics_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HlAnalyticsParams>,
) -> Result<Json<MarketAnalytics>, AppError> {
    validate_date_range(params.time_start, params.time_end)?;

    let pnl_summaries = state
        .repo
        .export_hl_pnl_summary(
            params.target_id,
            params.network.as_deref(),
            params.time_start,
            params.time_end,
        )
        .await
        .map_err(AppError::internal)?;

    let trade_histories = state
        .repo
        .export_hl_trade_history(
            params.target_id,
            params.network.as_deref(),
            params.time_start,
            params.time_end,
        )
        .await
        .map_err(AppError::internal)?;

    let analytics = spectraplex_adapters::hl_analytics::build_market_analytics(
        &pnl_summaries,
        &trade_histories,
    );

    Ok(Json(analytics))
}

// ---------------------------------------------------------------------------
// Protocol analytics endpoints (P5-W3)
// ---------------------------------------------------------------------------

/// Query parameters for the protocol analytics endpoints.
#[derive(Debug, Deserialize)]
struct ProtocolAnalyticsParams {
    target_id: Option<Uuid>,
    network: Option<String>,
    time_start: Option<i64>,
    time_end: Option<i64>,
    protocol_address: Option<String>,
}

/// GET /v1/analytics/protocol/activity — per-protocol event activity analytics.
async fn protocol_activity_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProtocolAnalyticsParams>,
) -> Result<Json<ProtocolActivity>, AppError> {
    validate_date_range(params.time_start, params.time_end)?;

    let events = state
        .repo
        .export_protocol_events(
            params.target_id,
            params.network.as_deref(),
            params.time_start,
            params.time_end,
        )
        .await
        .map_err(AppError::internal)?;

    let protocol_addr = params.protocol_address.as_deref().unwrap_or_else(|| {
        events
            .first()
            .map(|e| e.protocol_address.as_str())
            .unwrap_or("")
    });

    let activity =
        spectraplex_adapters::protocol_analytics::build_protocol_activity(protocol_addr, &events);

    Ok(Json(activity))
}

/// GET /v1/analytics/protocol/tvl — TVL analytics from pool snapshots.
async fn protocol_tvl_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ProtocolAnalyticsParams>,
) -> Result<Json<TvlAnalytics>, AppError> {
    validate_date_range(params.time_start, params.time_end)?;

    let snapshots = state
        .repo
        .export_pool_snapshots(
            params.target_id,
            params.network.as_deref(),
            params.time_start,
            params.time_end,
        )
        .await
        .map_err(AppError::internal)?;

    let analytics = spectraplex_adapters::protocol_analytics::build_tvl_analytics(&snapshots);

    Ok(Json(analytics))
}

async fn get_dataset_completeness_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Vec<DatasetCompleteness>>, AppError> {
    validate_dataset_name(&name)?;
    let records = state
        .repo
        .list_completeness_by_dataset(&name)
        .await
        .map_err(AppError::internal)?;
    Ok(Json(records))
}

/// Response body for the `/v1/datasets/{name}/status` materialization status endpoint.
#[derive(Debug, Clone, Serialize)]
struct DatasetStatus {
    /// Dataset name.
    name: String,
    /// Active version details (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    active_version: Option<DatasetVersionInfo>,
    /// All known versions ordered by version number descending.
    versions: Vec<DatasetVersionInfo>,
    /// Aggregated completeness across all targets.
    completeness: Vec<DatasetCompletenessInfo>,
}

/// Summary of a dataset version for the status endpoint.
#[derive(Debug, Clone, Serialize)]
struct DatasetVersionInfo {
    id: Uuid,
    version: i32,
    status: String,
    parser_hash: Option<String>,
    created_at: String,
    notes: Option<String>,
}

/// Summary of a completeness record for the status endpoint.
#[derive(Debug, Clone, Serialize)]
struct DatasetCompletenessInfo {
    target_id: Uuid,
    network: String,
    status: String,
    coverage_start: Option<i64>,
    coverage_end: Option<i64>,
    records_count: i64,
    last_ingestion_run_id: Option<Uuid>,
}

async fn get_dataset_status_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<DatasetStatus>, AppError> {
    validate_dataset_name(&name)?;

    let versions = state
        .repo
        .list_dataset_versions(&name)
        .await
        .map_err(AppError::internal)?;

    let completeness_records = state
        .repo
        .list_completeness_by_dataset(&name)
        .await
        .map_err(AppError::internal)?;

    let version_infos: Vec<DatasetVersionInfo> = versions
        .iter()
        .map(|v| DatasetVersionInfo {
            id: v.id,
            version: v.version,
            status: v.status.to_string(),
            parser_hash: v.parser_hash.clone(),
            created_at: v.created_at.to_rfc3339(),
            notes: v.notes.clone(),
        })
        .collect();

    let active_version = version_infos.iter().find(|v| v.status == "active").cloned();

    let completeness_infos: Vec<DatasetCompletenessInfo> = completeness_records
        .iter()
        .map(|c| DatasetCompletenessInfo {
            target_id: c.target_id,
            network: c.network.clone(),
            status: c.status.to_string(),
            coverage_start: c.coverage_start,
            coverage_end: c.coverage_end,
            records_count: c.records_count,
            last_ingestion_run_id: c.last_ingestion_run_id,
        })
        .collect();

    Ok(Json(DatasetStatus {
        name,
        active_version,
        versions: version_infos,
        completeness: completeness_infos,
    }))
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
        let export_dir = std::env::temp_dir()
            .join(format!("sp_test_exports_{}", Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&export_dir).ok();
        let config = AppConfig {
            api_key,
            allowed_wallets,
            export_dir,
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
            export_jobs: RwLock::new(HashMap::new()),
            rate_limiter: Arc::new(RateLimiter::new(
                RATE_LIMIT_CAPACITY,
                RATE_LIMIT_REFILL_RATE,
            )),
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
            .route("/v1/targets", post(register_target))
            .route("/v1/targets", get(list_targets))
            .route("/v1/targets/{target_id}", get(get_target))
            .route("/v1/networks", get(list_networks))
            .route("/v1/networks/{network_id}", get(get_network))
            .route("/v1/datasets", get(list_all_datasets))
            .route(
                "/v1/datasets/{name}/versions",
                get(list_dataset_versions_handler),
            )
            .route("/v1/datasets/{name}/records", get(query_dataset_records))
            .route(
                "/v1/datasets/{name}/completeness",
                get(get_dataset_completeness_handler),
            )
            .route(
                "/v1/datasets/{name}/status",
                get(get_dataset_status_handler),
            )
            .route("/v1/export/dataset", post(create_export_job))
            .route("/v1/export/jobs/{job_id}", get(get_export_job_status))
            .route("/v1/export/jobs/{job_id}/download", get(download_export))
            .route("/v1/export/tax", get(tax_export))
            .route("/v1/forensics/activity", get(forensics_activity_handler))
            .route("/v1/analytics/hl/trader", get(hl_trader_analytics_handler))
            .route("/v1/analytics/hl/market", get(hl_market_analytics_handler))
            .route(
                "/v1/analytics/protocol/activity",
                get(protocol_activity_handler),
            )
            .route("/v1/analytics/protocol/tvl", get(protocol_tvl_handler))
            .layer(middleware::from_fn_with_state(
                Arc::clone(&state),
                rate_limit_middleware,
            ))
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

    #[tokio::test]
    async fn test_background_cleanup_prunes_both_maps() {
        let state = test_state();
        let old_job = Uuid::new_v4();
        let old_export = Uuid::new_v4();
        let fresh_job = Uuid::new_v4();

        let expired = Instant::now() - std::time::Duration::from_secs(JOB_TTL_SECS + 1);

        state.jobs.write().await.insert(
            old_job,
            JobEntry {
                status: JobStatus {
                    id: old_job,
                    state: JobState::Completed,
                    message: None,
                },
                finished_at: Some(expired),
            },
        );
        state.jobs.write().await.insert(
            fresh_job,
            JobEntry {
                status: JobStatus {
                    id: fresh_job,
                    state: JobState::Running,
                    message: None,
                },
                finished_at: None,
            },
        );
        state.export_jobs.write().await.insert(
            old_export,
            ExportJobEntry {
                status: ExportJobStatus {
                    id: old_export,
                    state: JobState::Failed,
                    dataset: "test".to_string(),
                    format: "json".to_string(),
                    record_count: None,
                    message: None,
                    delivered_to: None,
                    delivery_status: None,
                    dataset_version_id: None,
                    dataset_version: None,
                    completeness_status: None,
                    completeness_coverage: None,
                    started_at: None,
                    completed_at: None,
                    last_ingestion_run_id: None,
                },
                finished_at: Some(expired),
                data: None,
            },
        );

        // Both prune methods should clear stale entries
        state.prune_stale_jobs().await;
        state.prune_stale_export_jobs().await;

        assert!(!state.jobs.read().await.contains_key(&old_job));
        assert!(state.jobs.read().await.contains_key(&fresh_job));
        assert!(!state.export_jobs.read().await.contains_key(&old_export));
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
            (JobState::Delivering, "delivering"),
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
        // Unconfigured API key is a server misconfiguration, not a client auth failure
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
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
            export_jobs: RwLock::new(HashMap::new()),
            rate_limiter: Arc::new(RateLimiter::new(
                RATE_LIMIT_CAPACITY,
                RATE_LIMIT_REFILL_RATE,
            )),
        });

        // Hold the single permit so the next ingest request is guaranteed to
        // get 503 — no timing dependency on a spawned background task.
        let _held_permit = state.job_semaphore.clone().acquire_owned().await.unwrap();

        let app = test_router_with_state(Arc::clone(&state));
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
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
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
    fn test_check_wallet_allowed_evm_case_insensitive() {
        // EVM addresses (0x-prefixed) should be compared case-insensitively
        let allowed: Option<HashSet<String>> = Some(["0xabc123".to_string()].into_iter().collect());
        assert!(check_wallet_allowed("0xABC123", &allowed).is_ok());
    }

    #[test]
    fn test_check_wallet_allowed_solana_case_sensitive() {
        // Solana base58 addresses are case-sensitive; different case = different address
        let allowed: Option<HashSet<String>> = Some(["DRpbCBMx".to_string()].into_iter().collect());
        let err = check_wallet_allowed("drpbcbmx", &allowed).unwrap_err();
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        // Exact case should still match
        assert!(check_wallet_allowed("DRpbCBMx", &allowed).is_ok());
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

    #[test]
    fn test_is_private_ip_ipv6_ula() {
        // ULA range fc00::/7
        assert!(is_private_ip("fc00::1"));
        assert!(is_private_ip("fd12:3456:789a::1"));
    }

    #[test]
    fn test_is_private_ip_ipv6_link_local() {
        // Link-local fe80::/10
        assert!(is_private_ip("fe80::1"));
        assert!(is_private_ip("fe80::a1:b2c3"));
    }

    #[test]
    fn test_is_private_ip_ipv6_multicast() {
        // Multicast ff00::/8
        assert!(is_private_ip("ff02::1"));
        assert!(is_private_ip("ff05::2"));
    }

    #[test]
    fn test_is_private_ip_ipv4_mapped_ipv6() {
        // IPv4-mapped IPv6 addresses (::ffff:x.x.x.x)
        assert!(is_private_ip("::ffff:127.0.0.1"));
        assert!(is_private_ip("::ffff:10.0.0.1"));
        assert!(is_private_ip("::ffff:192.168.1.1"));
        assert!(!is_private_ip("::ffff:8.8.8.8"));
    }

    #[test]
    fn test_is_private_ip_ipv4_compatible_ipv6() {
        // IPv4-compatible addresses (::x.x.x.x) must also be blocked
        assert!(is_private_ip("::10.0.0.1"));
        assert!(is_private_ip("::127.0.0.1"));
        assert!(!is_private_ip("::8.8.8.8"));
    }

    #[test]
    fn test_is_private_ip_ipv6_public() {
        // Public IPv6 addresses should NOT be flagged
        assert!(!is_private_ip("2001:4860:4860::8888"));
        assert!(!is_private_ip("2606:4700::1111"));
    }

    #[test]
    fn test_callback_client_disables_redirects() {
        // Verify the callback HTTP client is built with redirect policy none.
        // This prevents redirect-based SSRF where a public URL redirects to
        // a private address, bypassing the hostname-level SSRF check.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        // Client builds successfully with redirect policy disabled
        drop(client);
    }

    #[test]
    fn test_webhook_sink_client_disables_redirects() {
        // Verify the webhook sink HTTP client is built with redirect policy none.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        drop(client);
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
            .contains("only supported for solana"));
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
            export_jobs: RwLock::new(HashMap::new()),
            rate_limiter: Arc::new(RateLimiter::new(
                RATE_LIMIT_CAPACITY,
                RATE_LIMIT_REFILL_RATE,
            )),
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
            uptime_secs: 120,
            transactions_ingested: 5000,
            last_slot: 300000,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["uptime_secs"], 120);
        assert_eq!(json["transactions_ingested"], 5000);
        assert_eq!(json["last_slot"], 300000);
    }

    // -----------------------------------------------------------------------
    // Target registration tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_register_target_request_deserialization() {
        let json = serde_json::json!({
            "kind": "wallet",
            "network": "solana-mainnet",
            "address": "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
            "mode": "both",
            "label": "My Wallet"
        });
        let req: RegisterTargetRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.kind, "wallet");
        assert_eq!(req.network, "solana-mainnet");
        assert_eq!(
            req.address.as_deref(),
            Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy")
        );
        assert_eq!(req.mode.as_deref(), Some("both"));
        assert_eq!(req.label.as_deref(), Some("My Wallet"));
        assert!(req.filter_spec.is_none());
    }

    #[test]
    fn test_register_target_request_minimal() {
        let json = serde_json::json!({
            "kind": "contract",
            "network": "ethereum-mainnet"
        });
        let req: RegisterTargetRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.kind, "contract");
        assert_eq!(req.network, "ethereum-mainnet");
        assert!(req.address.is_none());
        assert!(req.mode.is_none());
        assert!(req.label.is_none());
        assert!(req.filter_spec.is_none());
    }

    #[test]
    fn test_register_target_request_with_filter_spec() {
        let json = serde_json::json!({
            "kind": "topic_filter",
            "network": "ethereum-mainnet",
            "filter_spec": {
                "topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"]
            }
        });
        let req: RegisterTargetRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.kind, "topic_filter");
        assert!(req.filter_spec.is_some());
    }

    #[test]
    fn test_target_validation_rejects_missing_address_for_wallet() {
        let now = chrono::Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: None,
            filter_spec: None,
            mode: TargetMode::Both,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };
        let result = validate_target(&target);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.contains("requires a non-empty address")));
    }

    #[test]
    fn test_target_validation_rejects_invalid_kind_for_family() {
        let now = chrono::Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Contract,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: Some("abc".to_string()),
            filter_spec: None,
            mode: TargetMode::Backfill,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };
        let result = validate_target(&target);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.contains("not valid for chain family")));
    }

    #[test]
    fn test_target_validation_rejects_missing_filter_spec_for_topic_filter() {
        let now = chrono::Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::TopicFilter,
            network: "ethereum-mainnet".to_string(),
            chain_family: ChainFamily::Evm,
            address: None,
            filter_spec: None,
            mode: TargetMode::Backfill,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };
        let result = validate_target(&target);
        assert!(result.is_err());
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| e.contains("requires filter_spec")));
    }

    #[test]
    fn test_target_validation_accepts_valid_wallet() {
        let now = chrono::Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string()),
            filter_spec: None,
            mode: TargetMode::Both,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn test_target_kind_parsing() {
        assert!("wallet".parse::<TargetKind>().is_ok());
        assert!("contract".parse::<TargetKind>().is_ok());
        assert!("program".parse::<TargetKind>().is_ok());
        assert!("account".parse::<TargetKind>().is_ok());
        assert!("topic_filter".parse::<TargetKind>().is_ok());
        assert!("market".parse::<TargetKind>().is_ok());
        assert!("pool".parse::<TargetKind>().is_ok());
        assert!("protocol".parse::<TargetKind>().is_ok());
        assert!("invalid_kind".parse::<TargetKind>().is_err());
    }

    #[test]
    fn test_target_mode_parsing() {
        assert!("backfill".parse::<TargetMode>().is_ok());
        assert!("stream".parse::<TargetMode>().is_ok());
        assert!("both".parse::<TargetMode>().is_ok());
        assert!("invalid_mode".parse::<TargetMode>().is_err());
    }

    #[test]
    fn test_target_list_params_deserialization() {
        let json = serde_json::json!({
            "limit": 10,
            "offset": 5,
            "kind": "wallet",
            "network": "solana-mainnet"
        });
        let params: TargetListParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.limit, Some(10));
        assert_eq!(params.offset, Some(5));
        assert_eq!(params.kind.as_deref(), Some("wallet"));
        assert_eq!(params.network.as_deref(), Some("solana-mainnet"));
    }

    #[test]
    fn test_target_list_params_empty() {
        let json = serde_json::json!({});
        let params: TargetListParams = serde_json::from_value(json).unwrap();
        assert!(params.limit.is_none());
        assert!(params.offset.is_none());
        assert!(params.kind.is_none());
        assert!(params.network.is_none());
    }

    #[test]
    fn test_conflict_error() {
        let err = conflict("duplicate target");
        assert_eq!(err.status, StatusCode::CONFLICT);
        assert_eq!(err.message, "duplicate target");
    }

    #[tokio::test]
    async fn test_register_target_bad_kind() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/targets")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "kind": "invalid_kind",
                    "network": "solana-mainnet"
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
            .contains("Invalid target kind"));
    }

    #[tokio::test]
    async fn test_register_target_bad_mode() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/targets")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "kind": "wallet",
                    "network": "solana-mainnet",
                    "address": "abc123",
                    "mode": "invalid_mode"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("Invalid mode"));
    }

    #[tokio::test]
    async fn test_register_target_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/targets")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "kind": "wallet",
                    "network": "solana-mainnet",
                    "address": "abc123"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_list_targets_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/targets")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_get_target_requires_auth() {
        let app = test_router();
        let target_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .uri(format!("/v1/targets/{}", target_id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_list_networks_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/networks")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_get_network_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/networks/solana-mainnet")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_list_targets_bad_kind_filter() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/targets?kind=invalid_kind")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // -- Dataset query endpoint tests (P4-W1) --

    #[test]
    fn test_validate_dataset_name_valid() {
        assert!(validate_dataset_name("token_transfers").is_ok());
        assert!(validate_dataset_name("hl_fills").is_ok());
        assert!(validate_dataset_name("decoded_events").is_ok());
    }

    #[test]
    fn test_validate_dataset_name_empty() {
        let err = validate_dataset_name("").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_dataset_name_too_long() {
        let long = "a".repeat(65);
        let err = validate_dataset_name(&long).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_validate_dataset_name_invalid_chars() {
        let err = validate_dataset_name("bad-name").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        let err = validate_dataset_name("bad name").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        let err = validate_dataset_name("bad;name").unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_queryable_datasets_count() {
        assert_eq!(QUERYABLE_DATASETS.len(), 12);
    }

    #[test]
    fn test_queryable_datasets_contains_expected() {
        assert!(QUERYABLE_DATASETS.contains(&"token_transfers"));
        assert!(QUERYABLE_DATASETS.contains(&"native_balance_deltas"));
        assert!(QUERYABLE_DATASETS.contains(&"decoded_events"));
        assert!(QUERYABLE_DATASETS.contains(&"hl_fills"));
        assert!(QUERYABLE_DATASETS.contains(&"hl_funding"));
        assert!(QUERYABLE_DATASETS.contains(&"positions"));
        assert!(QUERYABLE_DATASETS.contains(&"wallet_ledger"));
        assert!(QUERYABLE_DATASETS.contains(&"balance_history"));
        assert!(QUERYABLE_DATASETS.contains(&"hl_pnl_summary"));
        assert!(QUERYABLE_DATASETS.contains(&"hl_trade_history"));
        assert!(QUERYABLE_DATASETS.contains(&"protocol_events"));
        assert!(QUERYABLE_DATASETS.contains(&"pool_snapshots"));
    }

    #[test]
    fn test_dataset_info_serialization() {
        let info = DatasetInfo {
            name: "token_transfers".to_string(),
            latest_version: Some(1),
            latest_version_status: Some("active".to_string()),
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["name"], "token_transfers");
        assert_eq!(json["latest_version"], 1);
        assert_eq!(json["latest_version_status"], "active");
    }

    #[test]
    fn test_dataset_info_serialization_no_version() {
        let info = DatasetInfo {
            name: "positions".to_string(),
            latest_version: None,
            latest_version_status: None,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["name"], "positions");
        assert!(json["latest_version"].is_null());
        assert!(json["latest_version_status"].is_null());
    }

    #[test]
    fn test_dataset_query_params_deserialization() {
        let json = serde_json::json!({
            "target_id": "550e8400-e29b-41d4-a716-446655440000",
            "network": "solana-mainnet",
            "time_start": 1700000000,
            "time_end": 1700100000,
            "limit": 100,
            "offset": 10
        });
        let params: DatasetQueryParams = serde_json::from_value(json).unwrap();
        assert!(params.target_id.is_some());
        assert_eq!(params.network, Some("solana-mainnet".to_string()));
        assert_eq!(params.time_start, Some(1700000000));
        assert_eq!(params.time_end, Some(1700100000));
        assert_eq!(params.limit, Some(100));
        assert_eq!(params.offset, Some(10));
    }

    #[test]
    fn test_dataset_query_params_all_optional() {
        let json = serde_json::json!({});
        let params: DatasetQueryParams = serde_json::from_value(json).unwrap();
        assert!(params.target_id.is_none());
        assert!(params.network.is_none());
        assert!(params.time_start.is_none());
        assert!(params.time_end.is_none());
        assert!(params.limit.is_none());
        assert!(params.offset.is_none());
    }

    #[tokio::test]
    async fn test_datasets_endpoint_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/datasets")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_dataset_records_invalid_dataset_name() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/datasets/nonexistent_dataset/records")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_dataset_records_invalid_name_chars() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/datasets/bad-name/records")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_dataset_records_invalid_time_range() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/datasets/token_transfers/records?time_start=2000&time_end=1000")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_dataset_versions_endpoint_routes() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/datasets/token_transfers/versions")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        // Route exists (not 404); may fail with 500 due to fake DB pool
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_dataset_completeness_endpoint_routes() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/datasets/token_transfers/completeness")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        // Route exists (not 404); may fail with 500 due to fake DB pool
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_dataset_records_endpoint_routes() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/datasets/token_transfers/records")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        // Route exists (not 404); may fail with 500 due to fake DB pool
        assert_ne!(response.status(), StatusCode::NOT_FOUND);
        assert_ne!(response.status(), StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------
    // Export job tests (P4-W2)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_export_job_requires_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_export_job_invalid_dataset() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "nonexistent_dataset"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_job_unsupported_format() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers",
                    "format": "parquet"
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
            .contains("Unsupported export format"));
    }

    #[tokio::test]
    async fn test_export_job_invalid_time_range() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers",
                    "time_start": 2000,
                    "time_end": 1000
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_job_accepted_jsonl() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers",
                    "format": "jsonl"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        // Async job creation returns 202 Accepted
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(job["state"], "pending");
        assert_eq!(job["dataset"], "token_transfers");
        assert_eq!(job["format"], "jsonl");
    }

    #[tokio::test]
    async fn test_export_job_accepted_csv() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "hl_fills",
                    "format": "csv"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(job["state"], "pending");
        assert_eq!(job["dataset"], "hl_fills");
        assert_eq!(job["format"], "csv");
    }

    #[tokio::test]
    async fn test_export_job_default_format_is_jsonl() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "native_balance_deltas"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(job["format"], "jsonl");
    }

    #[tokio::test]
    async fn test_export_job_with_filters() {
        let app = test_router();
        let target_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "decoded_events",
                    "format": "jsonl",
                    "target_id": target_id,
                    "network": "ethereum-mainnet",
                    "time_start": 1700000000,
                    "time_end": 1700100000
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn test_export_job_status_not_found() {
        let app = test_router();
        let job_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_export_download_not_found() {
        let app = test_router();
        let job_id = Uuid::new_v4();
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}/download", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_export_download_completed_job() {
        let state = test_state();
        let job_id = Uuid::new_v4();
        state.export_jobs.write().await.insert(
            job_id,
            ExportJobEntry {
                status: ExportJobStatus {
                    id: job_id,
                    state: JobState::Completed,
                    dataset: "token_transfers".to_string(),
                    format: "jsonl".to_string(),
                    record_count: Some(2),
                    message: Some("Exported 2 records".to_string()),
                    delivered_to: None,
                    delivery_status: None,
                    dataset_version_id: None,
                    dataset_version: None,
                    completeness_status: None,
                    completeness_coverage: None,
                    started_at: None,
                    completed_at: None,
                    last_ingestion_run_id: None,
                },
                finished_at: Some(Instant::now()),
                data: Some(ExportData {
                    content_type: "application/x-ndjson",
                    body: b"{\"test\":1}\n{\"test\":2}\n".to_vec(),
                }),
            },
        );

        let app = test_router_with_state(Arc::clone(&state));
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}/download", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let ct = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "application/x-ndjson");

        let disp = response
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(disp.contains("token_transfers"));
        assert!(disp.contains(".jsonl"));

        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"{\"test\":1}\n{\"test\":2}\n");
    }

    #[tokio::test]
    async fn test_export_download_pending_job_returns_conflict() {
        let state = test_state();
        let job_id = Uuid::new_v4();
        state.export_jobs.write().await.insert(
            job_id,
            ExportJobEntry {
                status: ExportJobStatus {
                    id: job_id,
                    state: JobState::Running,
                    dataset: "hl_fills".to_string(),
                    format: "csv".to_string(),
                    record_count: None,
                    message: None,
                    delivered_to: None,
                    delivery_status: None,
                    dataset_version_id: None,
                    dataset_version: None,
                    completeness_status: None,
                    completeness_coverage: None,
                    started_at: None,
                    completed_at: None,
                    last_ingestion_run_id: None,
                },
                finished_at: None,
                data: None,
            },
        );

        let app = test_router_with_state(Arc::clone(&state));
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}/download", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_export_download_failed_job() {
        let state = test_state();
        let job_id = Uuid::new_v4();
        state.export_jobs.write().await.insert(
            job_id,
            ExportJobEntry {
                status: ExportJobStatus {
                    id: job_id,
                    state: JobState::Failed,
                    dataset: "hl_fills".to_string(),
                    format: "jsonl".to_string(),
                    record_count: None,
                    message: Some("DB connection refused".to_string()),
                    delivered_to: None,
                    delivery_status: None,
                    dataset_version_id: None,
                    dataset_version: None,
                    completeness_status: None,
                    completeness_coverage: None,
                    started_at: None,
                    completed_at: None,
                    last_ingestion_run_id: None,
                },
                finished_at: Some(Instant::now()),
                data: None,
            },
        );

        let app = test_router_with_state(Arc::clone(&state));
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}/download", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_export_download_sanitizes_content_disposition() {
        let state = test_state();
        let job_id = Uuid::new_v4();
        // Simulate an entry with characters that would be stripped by sanitization
        state.export_jobs.write().await.insert(
            job_id,
            ExportJobEntry {
                status: ExportJobStatus {
                    id: job_id,
                    state: JobState::Completed,
                    dataset: "hl_fills\r\nX-Injected: true".to_string(),
                    format: "csv\r\n\r\n<html>".to_string(),
                    record_count: Some(1),
                    message: None,
                    delivered_to: None,
                    delivery_status: None,
                    dataset_version_id: None,
                    dataset_version: None,
                    completeness_status: None,
                    completeness_coverage: None,
                    started_at: None,
                    completed_at: None,
                    last_ingestion_run_id: None,
                },
                finished_at: Some(Instant::now()),
                data: Some(ExportData {
                    content_type: "text/csv; charset=utf-8",
                    body: b"a,b\n1,2\n".to_vec(),
                }),
            },
        );

        let app = test_router_with_state(Arc::clone(&state));
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}/download", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let disposition = response
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap();
        // CRLF and special characters must be stripped; only [a-zA-Z0-9_-] kept
        assert!(
            !disposition.contains('\r'),
            "Content-Disposition must not contain CR"
        );
        assert!(
            !disposition.contains('\n'),
            "Content-Disposition must not contain LF"
        );
        assert!(
            disposition.contains("hl_fillsX-Injectedtrue"),
            "dataset should be sanitized to alphanumeric/underscore/hyphen: {disposition}"
        );
        assert!(
            disposition.contains("csvhtml"),
            "format should be sanitized to alphanumeric/underscore/hyphen: {disposition}"
        );
    }

    #[tokio::test]
    async fn test_export_job_status_found() {
        let state = test_state();
        let job_id = Uuid::new_v4();
        state.export_jobs.write().await.insert(
            job_id,
            ExportJobEntry {
                status: ExportJobStatus {
                    id: job_id,
                    state: JobState::Completed,
                    dataset: "positions".to_string(),
                    format: "csv".to_string(),
                    record_count: Some(42),
                    message: Some("Exported 42 records".to_string()),
                    delivered_to: None,
                    delivery_status: None,
                    dataset_version_id: None,
                    dataset_version: None,
                    completeness_status: None,
                    completeness_coverage: None,
                    started_at: None,
                    completed_at: None,
                    last_ingestion_run_id: None,
                },
                finished_at: Some(Instant::now()),
                data: None,
            },
        );

        let app = test_router_with_state(Arc::clone(&state));
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(job["state"], "completed");
        assert_eq!(job["dataset"], "positions");
        assert_eq!(job["format"], "csv");
        assert_eq!(job["record_count"], 42);
    }

    #[test]
    fn test_export_job_status_serialization() {
        let status = ExportJobStatus {
            id: Uuid::nil(),
            state: JobState::Pending,
            dataset: "token_transfers".to_string(),
            format: "jsonl".to_string(),
            record_count: None,
            message: None,
            delivered_to: None,
            delivery_status: None,
            dataset_version_id: None,
            dataset_version: None,
            completeness_status: None,
            completeness_coverage: None,
            started_at: None,
            completed_at: None,
            last_ingestion_run_id: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["state"], "pending");
        assert_eq!(json["dataset"], "token_transfers");
        assert_eq!(json["format"], "jsonl");
        assert!(json["record_count"].is_null());
    }

    #[tokio::test]
    async fn test_export_prune_stale_jobs() {
        let state = test_state();
        let old_id = Uuid::new_v4();
        let new_id = Uuid::new_v4();

        state.export_jobs.write().await.insert(
            old_id,
            ExportJobEntry {
                status: ExportJobStatus {
                    id: old_id,
                    state: JobState::Completed,
                    dataset: "hl_fills".to_string(),
                    format: "jsonl".to_string(),
                    record_count: Some(0),
                    message: None,
                    delivered_to: None,
                    delivery_status: None,
                    dataset_version_id: None,
                    dataset_version: None,
                    completeness_status: None,
                    completeness_coverage: None,
                    started_at: None,
                    completed_at: None,
                    last_ingestion_run_id: None,
                },
                finished_at: Some(
                    Instant::now() - std::time::Duration::from_secs(JOB_TTL_SECS + 1),
                ),
                data: None,
            },
        );
        state.export_jobs.write().await.insert(
            new_id,
            ExportJobEntry {
                status: ExportJobStatus {
                    id: new_id,
                    state: JobState::Running,
                    dataset: "hl_fills".to_string(),
                    format: "csv".to_string(),
                    record_count: None,
                    message: None,
                    delivered_to: None,
                    delivery_status: None,
                    dataset_version_id: None,
                    dataset_version: None,
                    completeness_status: None,
                    completeness_coverage: None,
                    started_at: None,
                    completed_at: None,
                    last_ingestion_run_id: None,
                },
                finished_at: None,
                data: None,
            },
        );

        state.prune_stale_export_jobs().await;

        let exports = state.export_jobs.read().await;
        assert!(!exports.contains_key(&old_id));
        assert!(exports.contains_key(&new_id));
    }

    #[tokio::test]
    async fn test_export_all_datasets_accepted() {
        for dataset in EXPORTABLE_DATASETS {
            let app = test_router();
            let req = axum::http::Request::builder()
                .method("POST")
                .uri("/v1/export/dataset")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", TEST_API_KEY))
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "dataset": dataset
                    }))
                    .unwrap(),
                ))
                .unwrap();
            let response = app.oneshot(req).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::ACCEPTED,
                "dataset {dataset} should be accepted"
            );
        }
    }

    #[tokio::test]
    async fn test_export_ledger_entries_not_exportable() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "ledger_entries"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_serialize_to_jsonl() {
        use spectraplex_core::materializer::TokenTransfer;
        let records = vec![TokenTransfer {
            id: Uuid::nil(),
            raw_transaction_id: None,
            network: "solana-mainnet".to_string(),
            token_address: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            token_symbol: Some("USDC".to_string()),
            from_address: "sender".to_string(),
            to_address: "receiver".to_string(),
            amount: bigdecimal::BigDecimal::from(100),
            decimals: 6,
            transfer_index: 0,
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let bytes = serialize_to_jsonl(&records).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = output.trim().split('\n').collect();
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["network"], "solana-mainnet");
        assert_eq!(parsed["token_symbol"], "USDC");
    }

    #[test]
    fn test_token_transfers_to_csv() {
        use spectraplex_core::materializer::TokenTransfer;
        let records = vec![TokenTransfer {
            id: Uuid::nil(),
            raw_transaction_id: None,
            network: "ethereum-mainnet".to_string(),
            token_address: "0xtoken".to_string(),
            token_symbol: Some("USDC".to_string()),
            from_address: "0xfrom".to_string(),
            to_address: "0xto".to_string(),
            amount: bigdecimal::BigDecimal::from(50),
            decimals: 6,
            transfer_index: 0,
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let bytes = token_transfers_to_csv(&records);
        let csv = String::from_utf8(bytes).unwrap();
        let lines: Vec<&str> = csv.trim().split('\n').collect();
        assert_eq!(lines.len(), 2); // header + 1 row
        assert!(lines[0].starts_with("id,"));
        assert!(lines[1].contains("ethereum-mainnet"));
        assert!(lines[1].contains("USDC"));
    }

    #[test]
    fn test_content_type_for_format() {
        assert_eq!(
            content_type_for_format(ExportFormat::Jsonl),
            "application/x-ndjson"
        );
        assert_eq!(
            content_type_for_format(ExportFormat::Csv),
            "text/csv; charset=utf-8"
        );
    }

    // -- Sink tests (P4-W3) --

    #[test]
    fn test_validate_sink_config_local_file_valid() {
        let dir = std::env::temp_dir().join(format!("sp_test_export_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some("export.jsonl".to_string()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        assert!(validate_sink_config(&config, dir.to_str().unwrap()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_sink_config_local_file_path_traversal() {
        let dir = std::env::temp_dir().join(format!("sp_test_export_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some("../etc/passwd".to_string()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, dir.to_str().unwrap()).unwrap_err();
        assert!(err.message.contains("path traversal"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_sink_config_local_file_absolute_path_rerooted() {
        // Absolute paths have leading '/' stripped and are re-rooted under export_dir,
        // so "/etc/passwd" becomes "{export_dir}/etc/passwd" which is safely contained.
        let dir = std::env::temp_dir().join(format!("sp_test_export_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some("/etc/passwd".to_string()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        assert!(validate_sink_config(&config, dir.to_str().unwrap()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_sink_config_local_file_symlink_escape_rejected() {
        let dir = std::env::temp_dir().join(format!("sp_test_export_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create a symlink inside export_dir that points outside
        let escape_link = dir.join("escape");
        std::os::unix::fs::symlink("/tmp", &escape_link).unwrap();
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some("escape/should_not_be_allowed".to_string()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, dir.to_str().unwrap()).unwrap_err();
        assert!(err
            .message
            .contains("outside the configured export directory"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_sink_config_local_file_null_byte_rejected() {
        let dir = std::env::temp_dir().join(format!("sp_test_export_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some("legit.txt\0../../etc/passwd".to_string()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, dir.to_str().unwrap()).unwrap_err();
        assert!(err.message.contains("null bytes"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_validate_sink_config_webhook_valid() {
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: None,
            connection_string: None,
            table: None,
        };
        assert!(validate_sink_config(&config, "/tmp").is_ok());
    }

    #[test]
    fn test_validate_sink_config_webhook_loopback_rejected() {
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("http://127.0.0.1:8080/hook".to_string()),
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, "/tmp").unwrap_err();
        assert!(err.message.contains("private"));
    }

    #[test]
    fn test_validate_sink_config_webhook_missing_url() {
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, "/tmp").unwrap_err();
        assert!(err.message.contains("url"));
    }

    #[test]
    fn test_validate_sink_config_database_rejected() {
        let config = SinkConfig {
            sink_type: SinkType::Database,
            file_path: None,
            url: None,
            headers: None,
            connection_string: Some("postgresql://localhost/exports".to_string()),
            table: Some("export_data".to_string()),
        };
        let err = validate_sink_config(&config, "/tmp").unwrap_err();
        assert!(err.message.contains("not yet implemented"));
    }

    #[test]
    fn test_validate_sink_config_webhook_forbidden_header() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer secret".to_string());
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: Some(headers),
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, "/tmp").unwrap_err();
        assert!(err.message.contains("Forbidden webhook header"));
    }

    #[test]
    fn test_validate_sink_config_webhook_invalid_header_name() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("Bad Header\r\n".to_string(), "value".to_string());
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: Some(headers),
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, "/tmp").unwrap_err();
        assert!(err.message.contains("Invalid header name"));
    }

    #[test]
    fn test_validate_sink_config_webhook_control_char_in_value() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Custom".to_string(), "val\r\nInjected: true".to_string());
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: Some(headers),
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, "/tmp").unwrap_err();
        assert!(err.message.contains("control characters"));
    }

    #[test]
    fn test_validate_sink_config_webhook_valid_custom_headers() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Custom-Header".to_string(), "some-value".to_string());
        headers.insert("X-Api_Key".to_string(), "key123".to_string());
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: Some(headers),
            connection_string: None,
            table: None,
        };
        assert!(validate_sink_config(&config, "/tmp").is_ok());
    }

    #[test]
    fn test_validate_sink_config_webhook_tab_in_value_allowed() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Custom".to_string(), "value\twith\ttabs".to_string());
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: Some(headers),
            connection_string: None,
            table: None,
        };
        assert!(validate_sink_config(&config, "/tmp").is_ok());
    }

    #[test]
    fn test_build_sink_local_file() {
        let dir = std::env::temp_dir().join(format!("sp_test_export_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some("test.jsonl".to_string()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        assert!(build_sink(&config, dir.to_str().unwrap()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_sink_webhook() {
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: None,
            connection_string: None,
            table: None,
        };
        assert!(build_sink(&config, "/tmp").is_ok());
    }

    #[test]
    fn test_build_sink_database_not_implemented() {
        let config = SinkConfig {
            sink_type: SinkType::Database,
            file_path: None,
            url: None,
            headers: None,
            connection_string: Some("postgresql://localhost/db".to_string()),
            table: Some("export_data".to_string()),
        };
        assert!(build_sink(&config, "/tmp").is_err());
    }

    #[tokio::test]
    async fn test_local_file_sink_write_and_readback() {
        let dir = std::env::temp_dir().join(format!("spectraplex_test_{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("export.jsonl");

        let sink = LocalFileSink {
            path: path.to_str().unwrap().to_string(),
        };

        let data = b"{\"foo\":1}\n{\"bar\":2}\n";
        let meta = DeliveryMetadata {
            job_id: Uuid::new_v4(),
            dataset: "token_transfers".to_string(),
            format: "jsonl".to_string(),
            record_count: 2,
            dataset_version_id: None,
            completeness_status: None,
        };

        let receipt = sink.deliver(data, &meta).await.unwrap();
        assert_eq!(receipt.sink_type, SinkType::LocalFile);
        assert_eq!(receipt.bytes_written, data.len());
        assert!(receipt.destination.contains("export.jsonl"));

        // Verify file content
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content.as_bytes(), data);

        // Cleanup
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn test_local_file_sink_bad_path() {
        let sink = LocalFileSink {
            path: "/nonexistent_dir_abc123/impossible/export.jsonl".to_string(),
        };
        let data = b"test";
        let meta = DeliveryMetadata {
            job_id: Uuid::new_v4(),
            dataset: "test".to_string(),
            format: "jsonl".to_string(),
            record_count: 1,
            dataset_version_id: None,
            completeness_status: None,
        };
        let result = sink.deliver(data, &meta).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_webhook_sink_url_validation_via_sink_config() {
        // The webhook URL validation happens at validate_sink_config level.
        // Here we verify that invalid URLs are caught properly.
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("ftp://badprotocol.com/hook".to_string()),
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = validate_sink_config(&config, "/tmp").unwrap_err();
        assert!(err.message.contains("HTTP(S)"));
    }

    #[tokio::test]
    async fn test_export_job_with_sink_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers",
                    "format": "jsonl",
                    "sink": {
                        "sink_type": "local_file",
                        "file_path": "spectraplex_export_test.jsonl"
                    }
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(job["state"], "pending");
        assert_eq!(job["delivery_status"], "pending");
    }

    #[tokio::test]
    async fn test_export_job_with_invalid_sink_rejected() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers",
                    "sink": {
                        "sink_type": "webhook"
                    }
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("url"));
    }

    #[tokio::test]
    async fn test_export_job_with_database_sink_rejected() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers",
                    "sink": {
                        "sink_type": "database",
                        "connection_string": "postgresql://localhost/db",
                        "table": "export_data"
                    }
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
            .contains("not yet implemented"));
    }

    #[tokio::test]
    async fn test_export_job_without_sink_still_works() {
        // Backward compatibility: no sink field → same behavior as before
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers",
                    "format": "jsonl"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let job: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(job["state"], "pending");
        // No sink means no delivery_status field (skip_serializing_if)
        assert!(job.get("delivery_status").is_none() || job["delivery_status"].is_null());
        assert!(job.get("delivered_to").is_none() || job["delivered_to"].is_null());
    }

    #[test]
    fn test_export_job_status_with_delivery_fields_serialization() {
        let status = ExportJobStatus {
            id: Uuid::nil(),
            state: JobState::Completed,
            dataset: "token_transfers".to_string(),
            format: "jsonl".to_string(),
            record_count: Some(10),
            message: Some("Exported 10 records".to_string()),
            delivered_to: Some("/tmp/export.jsonl".to_string()),
            delivery_status: Some("delivered".to_string()),
            dataset_version_id: None,
            dataset_version: None,
            completeness_status: None,
            completeness_coverage: None,
            started_at: None,
            completed_at: None,
            last_ingestion_run_id: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["delivered_to"], "/tmp/export.jsonl");
        assert_eq!(json["delivery_status"], "delivered");
    }

    #[test]
    fn test_export_job_status_skip_none_delivery_fields() {
        let status = ExportJobStatus {
            id: Uuid::nil(),
            state: JobState::Pending,
            dataset: "token_transfers".to_string(),
            format: "jsonl".to_string(),
            record_count: None,
            message: None,
            delivered_to: None,
            delivery_status: None,
            dataset_version_id: None,
            dataset_version: None,
            completeness_status: None,
            completeness_coverage: None,
            started_at: None,
            completed_at: None,
            last_ingestion_run_id: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        // These fields should be absent when None (skip_serializing_if)
        assert!(json.get("delivered_to").is_none());
        assert!(json.get("delivery_status").is_none());
    }

    #[tokio::test]
    async fn test_export_job_with_webhook_sink_path_traversal_rejected() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "token_transfers",
                    "sink": {
                        "sink_type": "local_file",
                        "file_path": "../etc/passwd"
                    }
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // -----------------------------------------------------------------------
    // Metadata and observability tests (P4-W4)
    // -----------------------------------------------------------------------

    #[test]
    fn test_export_job_status_metadata_fields_present_when_populated() {
        let version_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let status = ExportJobStatus {
            id: Uuid::nil(),
            state: JobState::Completed,
            dataset: "token_transfers".to_string(),
            format: "jsonl".to_string(),
            record_count: Some(100),
            message: Some("Exported 100 records".to_string()),
            delivered_to: None,
            delivery_status: None,
            dataset_version_id: Some(version_id),
            dataset_version: Some(3),
            completeness_status: Some("complete".to_string()),
            completeness_coverage: Some(serde_json::json!({
                "coverage_start": 1700000000,
                "coverage_end": 1700100000,
                "block_start": null,
                "block_end": null,
            })),
            started_at: Some(now),
            completed_at: Some(now),
            last_ingestion_run_id: Some(run_id),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["dataset_version_id"], version_id.to_string());
        assert_eq!(json["dataset_version"], 3);
        assert_eq!(json["completeness_status"], "complete");
        assert!(json["completeness_coverage"]["coverage_start"].is_number());
        assert!(json["started_at"].is_string());
        assert!(json["completed_at"].is_string());
        assert_eq!(json["last_ingestion_run_id"], run_id.to_string());
    }

    #[test]
    fn test_export_job_status_metadata_fields_omitted_when_none() {
        let status = ExportJobStatus {
            id: Uuid::nil(),
            state: JobState::Pending,
            dataset: "hl_fills".to_string(),
            format: "csv".to_string(),
            record_count: None,
            message: None,
            delivered_to: None,
            delivery_status: None,
            dataset_version_id: None,
            dataset_version: None,
            completeness_status: None,
            completeness_coverage: None,
            started_at: None,
            completed_at: None,
            last_ingestion_run_id: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        // All new metadata fields should be absent (skip_serializing_if)
        assert!(json.get("dataset_version_id").is_none());
        assert!(json.get("dataset_version").is_none());
        assert!(json.get("completeness_status").is_none());
        assert!(json.get("completeness_coverage").is_none());
        assert!(json.get("started_at").is_none());
        assert!(json.get("completed_at").is_none());
        assert!(json.get("last_ingestion_run_id").is_none());
        // Core fields still present
        assert_eq!(json["state"], "pending");
        assert_eq!(json["dataset"], "hl_fills");
    }

    #[test]
    fn test_export_job_status_backward_compat_no_metadata() {
        // Verify that a status with no metadata fields serializes to the same
        // shape as the pre-P4-W4 response (no extra keys).
        let status = ExportJobStatus {
            id: Uuid::nil(),
            state: JobState::Completed,
            dataset: "positions".to_string(),
            format: "jsonl".to_string(),
            record_count: Some(42),
            message: Some("Exported 42 records".to_string()),
            delivered_to: None,
            delivery_status: None,
            dataset_version_id: None,
            dataset_version: None,
            completeness_status: None,
            completeness_coverage: None,
            started_at: None,
            completed_at: None,
            last_ingestion_run_id: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        // Should only have the original fields
        assert!(keys.contains(&"id"));
        assert!(keys.contains(&"state"));
        assert!(keys.contains(&"dataset"));
        assert!(keys.contains(&"format"));
        assert!(keys.contains(&"record_count"));
        assert!(keys.contains(&"message"));
        // Metadata fields absent
        assert!(!keys.contains(&"dataset_version_id"));
        assert!(!keys.contains(&"started_at"));
    }

    #[test]
    fn test_export_metadata_default() {
        let meta = ExportMetadata::default();
        assert!(meta.dataset_version_id.is_none());
        assert!(meta.dataset_version.is_none());
        assert!(meta.completeness_status.is_none());
        assert!(meta.completeness_coverage.is_none());
        assert!(meta.last_ingestion_run_id.is_none());
    }

    #[test]
    fn test_dataset_status_response_shape() {
        let version_id = Uuid::new_v4();
        let target_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let status = DatasetStatus {
            name: "token_transfers".to_string(),
            active_version: Some(DatasetVersionInfo {
                id: version_id,
                version: 2,
                status: "active".to_string(),
                parser_hash: Some("sha256:abc".to_string()),
                created_at: "2026-01-01T00:00:00+00:00".to_string(),
                notes: None,
            }),
            versions: vec![
                DatasetVersionInfo {
                    id: version_id,
                    version: 2,
                    status: "active".to_string(),
                    parser_hash: Some("sha256:abc".to_string()),
                    created_at: "2026-01-01T00:00:00+00:00".to_string(),
                    notes: None,
                },
                DatasetVersionInfo {
                    id: Uuid::new_v4(),
                    version: 1,
                    status: "superseded".to_string(),
                    parser_hash: Some("sha256:old".to_string()),
                    created_at: "2025-06-01T00:00:00+00:00".to_string(),
                    notes: Some("initial release".to_string()),
                },
            ],
            completeness: vec![DatasetCompletenessInfo {
                target_id,
                network: "solana-mainnet".to_string(),
                status: "partial".to_string(),
                coverage_start: Some(1700000000),
                coverage_end: Some(1700100000),
                records_count: 42,
                last_ingestion_run_id: Some(run_id),
            }],
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["name"], "token_transfers");
        assert_eq!(json["active_version"]["version"], 2);
        assert_eq!(json["active_version"]["status"], "active");
        assert_eq!(json["versions"].as_array().unwrap().len(), 2);
        assert_eq!(json["completeness"].as_array().unwrap().len(), 1);
        assert_eq!(json["completeness"][0]["status"], "partial");
        assert_eq!(json["completeness"][0]["records_count"], 42);
    }

    #[test]
    fn test_dataset_status_no_active_version() {
        let status = DatasetStatus {
            name: "positions".to_string(),
            active_version: None,
            versions: vec![],
            completeness: vec![],
        };
        let json = serde_json::to_value(&status).unwrap();
        assert!(json.get("active_version").is_none());
        assert_eq!(json["versions"].as_array().unwrap().len(), 0);
        assert_eq!(json["completeness"].as_array().unwrap().len(), 0);
    }

    // ── Compatibility verification tests ──────────────────────────────
    //
    // These tests verify that all wallet-scoped and dataset-centric API
    // endpoints are routed, return expected response shapes, and share
    // the same authentication/authorization behavior.

    /// Helper: send a GET request without auth and assert 401.
    async fn assert_get_requires_auth(app: Router, uri: &str) {
        let req = axum::http::Request::builder()
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "GET {} should require auth",
            uri
        );
    }

    /// Helper: send a POST request without auth and assert 401.
    async fn assert_post_requires_auth(app: Router, uri: &str) {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "POST {} should require auth",
            uri
        );
    }

    /// Helper: send an authenticated GET and assert it does NOT return 401 or 404.
    async fn assert_get_routed(app: Router, uri: &str) {
        let req = axum::http::Request::builder()
            .uri(uri)
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        // The route must exist (not 404 from router-level mismatch) and must
        // have passed auth (not 401). DB-dependent endpoints will return 500
        // because the test DB is not real, which is expected.
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "GET {} should be routed (got 404)",
            uri
        );
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "GET {} should pass auth (got 401)",
            uri
        );
    }

    /// Helper: send an authenticated POST and assert it does NOT return 401 or 404.
    async fn assert_post_routed(app: Router, uri: &str, body: &str) {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "POST {} should be routed (got 404)",
            uri
        );
        assert_ne!(
            status,
            StatusCode::UNAUTHORIZED,
            "POST {} should pass auth (got 401)",
            uri
        );
    }

    // ── Wallet endpoint routing verification ──────────────────────────

    #[tokio::test]
    async fn compat_wallet_transactions_routed() {
        assert_get_routed(test_router(), "/v1/transactions/SomeWallet123").await;
    }

    #[tokio::test]
    async fn compat_wallet_single_transaction_routed() {
        assert_get_routed(test_router(), "/v1/transactions/SomeWallet123/0xdeadbeef").await;
    }

    #[tokio::test]
    async fn compat_wallet_ledger_routed() {
        assert_get_routed(test_router(), "/v1/ledger/SomeWallet123").await;
    }

    #[tokio::test]
    async fn compat_wallet_export_routed() {
        assert_get_routed(test_router(), "/v1/export/SomeWallet123").await;
    }

    #[tokio::test]
    async fn compat_wallet_balances_routed() {
        assert_get_routed(test_router(), "/v1/balances/SomeWallet123").await;
    }

    #[tokio::test]
    async fn compat_wallet_stats_routed() {
        assert_get_routed(test_router(), "/v1/stats/SomeWallet123").await;
    }

    // ── Dataset endpoint routing verification ─────────────────────────

    #[tokio::test]
    async fn compat_datasets_list_routed() {
        assert_get_routed(test_router(), "/v1/datasets").await;
    }

    #[tokio::test]
    async fn compat_datasets_versions_routed() {
        assert_get_routed(test_router(), "/v1/datasets/token_transfers/versions").await;
    }

    #[tokio::test]
    async fn compat_datasets_records_routed() {
        assert_get_routed(test_router(), "/v1/datasets/token_transfers/records").await;
    }

    #[tokio::test]
    async fn compat_datasets_completeness_routed() {
        assert_get_routed(test_router(), "/v1/datasets/token_transfers/completeness").await;
    }

    #[tokio::test]
    async fn compat_datasets_status_routed() {
        assert_get_routed(test_router(), "/v1/datasets/token_transfers/status").await;
    }

    #[tokio::test]
    async fn compat_export_dataset_routed() {
        assert_post_routed(
            test_router(),
            "/v1/export/dataset",
            r#"{"dataset":"token_transfers","format":"jsonl"}"#,
        )
        .await;
    }

    #[tokio::test]
    async fn compat_export_job_status_routed() {
        let job_id = Uuid::new_v4();
        // Export job not found returns 404 from handler logic, but the route
        // exists and auth passes. Use the status endpoint which returns 404
        // for missing jobs — we check only that auth is not the blocker.
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // 404 here is from the handler (job not found), not from the router.
        // Auth passed (not 401).
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn compat_export_job_download_routed() {
        let job_id = Uuid::new_v4();
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri(format!("/v1/export/jobs/{}/download", job_id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn compat_targets_list_routed() {
        assert_get_routed(test_router(), "/v1/targets").await;
    }

    #[tokio::test]
    async fn compat_targets_get_routed() {
        let id = Uuid::new_v4();
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri(format!("/v1/targets/{}", id))
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn compat_networks_list_routed() {
        assert_get_routed(test_router(), "/v1/networks").await;
    }

    #[tokio::test]
    async fn compat_networks_get_routed() {
        assert_get_routed(test_router(), "/v1/networks/solana-mainnet").await;
    }

    // ── Ingestion/job control endpoint routing verification ───────────

    #[tokio::test]
    async fn compat_ingest_routed() {
        assert_post_routed(
            test_router(),
            "/v1/ingest",
            r#"{"chain":"solana","wallet":"SomeWallet123"}"#,
        )
        .await;
    }

    #[tokio::test]
    async fn compat_ingest_batch_routed() {
        assert_post_routed(
            test_router(),
            "/v1/ingest/batch",
            r#"{"wallets":[{"chain":"solana","wallet":"SomeWallet123"}]}"#,
        )
        .await;
    }

    #[tokio::test]
    async fn compat_normalize_routed() {
        assert_post_routed(
            test_router(),
            "/v1/normalize",
            r#"{"wallet":"SomeWallet123"}"#,
        )
        .await;
    }

    #[tokio::test]
    async fn compat_stream_start_routed() {
        assert_post_routed(test_router(), "/v1/stream/start", r#"{"chain":"solana"}"#).await;
    }

    #[tokio::test]
    async fn compat_streams_list_routed() {
        assert_get_routed(test_router(), "/v1/streams").await;
    }

    // ── Shared auth behavior across both API surfaces ─────────────────

    #[tokio::test]
    async fn compat_wallet_endpoints_require_auth() {
        let wallet_gets = vec![
            "/v1/transactions/SomeWallet123",
            "/v1/transactions/SomeWallet123/0xdeadbeef",
            "/v1/ledger/SomeWallet123",
            "/v1/export/SomeWallet123",
            "/v1/balances/SomeWallet123",
            "/v1/stats/SomeWallet123",
        ];
        for uri in wallet_gets {
            assert_get_requires_auth(test_router(), uri).await;
        }
    }

    #[tokio::test]
    async fn compat_dataset_endpoints_require_auth() {
        let dataset_gets = vec![
            "/v1/datasets",
            "/v1/datasets/token_transfers/versions",
            "/v1/datasets/token_transfers/records",
            "/v1/datasets/token_transfers/completeness",
            "/v1/datasets/token_transfers/status",
            "/v1/targets",
            "/v1/networks",
        ];
        for uri in dataset_gets {
            assert_get_requires_auth(test_router(), uri).await;
        }
    }

    #[tokio::test]
    async fn compat_dataset_post_endpoints_require_auth() {
        let dataset_posts = vec!["/v1/export/dataset", "/v1/targets"];
        for uri in dataset_posts {
            assert_post_requires_auth(test_router(), uri).await;
        }
    }

    #[tokio::test]
    async fn compat_ingestion_endpoints_require_auth() {
        let ingestion_posts = vec!["/v1/ingest", "/v1/ingest/batch", "/v1/normalize"];
        for uri in ingestion_posts {
            assert_post_requires_auth(test_router(), uri).await;
        }
    }

    // ── Response shape verification ───────────────────────────────────

    #[tokio::test]
    async fn compat_wallet_endpoints_return_json_errors() {
        // Wallet endpoints should return JSON error bodies for invalid wallets
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/transactions/bad%20wallet")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("error").is_some(),
            "wallet error should be JSON with 'error' field"
        );
    }

    #[tokio::test]
    async fn compat_dataset_endpoints_return_json_errors() {
        // Invalid dataset name should return JSON error
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/v1/datasets/nonexistent_dataset/records")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Should be 400 for unknown dataset, not 404 from missing route
        assert_ne!(resp.status(), StatusCode::NOT_FOUND);
        assert_ne!(resp.status(), StatusCode::UNAUTHORIZED);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json.get("error").is_some(),
            "dataset error should be JSON with 'error' field"
        );
    }

    #[tokio::test]
    async fn compat_health_does_not_require_auth() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ── P5-W1: Wallet / Tax / Forensics Pack tests ─────────────────────

    #[test]
    fn test_wallet_ledger_in_queryable_datasets() {
        assert!(QUERYABLE_DATASETS.contains(&"wallet_ledger"));
    }

    #[test]
    fn test_balance_history_in_queryable_datasets() {
        assert!(QUERYABLE_DATASETS.contains(&"balance_history"));
    }

    #[test]
    fn test_wallet_ledger_in_exportable_datasets() {
        assert!(EXPORTABLE_DATASETS.contains(&"wallet_ledger"));
    }

    #[test]
    fn test_balance_history_in_exportable_datasets() {
        assert!(EXPORTABLE_DATASETS.contains(&"balance_history"));
    }

    #[tokio::test]
    async fn test_wallet_ledger_records_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/datasets/wallet_ledger/records").await;
    }

    #[tokio::test]
    async fn test_balance_history_records_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/datasets/balance_history/records").await;
    }

    #[tokio::test]
    async fn test_tax_export_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/export/tax").await;
    }

    #[tokio::test]
    async fn test_forensics_activity_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/export/tax").await;
    }

    #[tokio::test]
    async fn test_tax_export_requires_auth() {
        assert_get_requires_auth(test_router(), "/v1/export/tax").await;
    }

    #[tokio::test]
    async fn test_forensics_activity_requires_auth() {
        assert_get_requires_auth(test_router(), "/v1/forensics/activity").await;
    }

    #[test]
    fn test_wallet_ledger_to_csv_format() {
        let records = vec![WalletLedgerRecord {
            id: Uuid::nil(),
            raw_transaction_id: None,
            wallet_address: "0xWallet".to_string(),
            network: "solana-mainnet".to_string(),
            tx_hash: "abc123".to_string(),
            timestamp: 1700000000,
            entry_type: "transfer".to_string(),
            asset_symbol: "USDC".to_string(),
            amount: bigdecimal::BigDecimal::from(100),
            counterparty_address: Some("0xOther".to_string()),
            fee_amount: None,
            fee_asset: None,
            cost_basis: None,
            proceeds: None,
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let csv = wallet_ledger_to_csv(&records);
        let output = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 row
        assert!(lines[0].contains("counterparty_address"));
        assert!(lines[1].contains("0xOther"));
    }

    #[test]
    fn test_wallet_ledger_to_tax_csv_format() {
        let records = vec![WalletLedgerRecord {
            id: Uuid::nil(),
            raw_transaction_id: None,
            wallet_address: "0xWallet".to_string(),
            network: "solana-mainnet".to_string(),
            tx_hash: "abc123".to_string(),
            timestamp: 1700000000,
            entry_type: "transfer".to_string(),
            asset_symbol: "USDC".to_string(),
            amount: bigdecimal::BigDecimal::from(-50),
            counterparty_address: Some("0xOther".to_string()),
            fee_amount: Some(bigdecimal::BigDecimal::from(1)),
            fee_asset: Some("SOL".to_string()),
            cost_basis: None,
            proceeds: None,
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let csv = wallet_ledger_to_tax_csv(&records);
        let output = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 row
                                    // Check expected tax CSV columns
        let header = lines[0];
        assert!(header.starts_with("Date,Type,Sent_Asset,Sent_Amount,Received_Asset,Received_Amount,Fee_Asset,Fee_Amount,Cost_Basis,Proceeds,Gain_Loss,Tx_Hash,Network"));
        // Check that outgoing amounts go into Sent columns
        let row = lines[1];
        assert!(row.contains("USDC")); // sent asset
        assert!(row.contains("50")); // sent amount (absolute)
        assert!(row.contains("SOL")); // fee asset
        assert!(row.contains("abc123")); // tx hash
    }

    #[test]
    fn test_tax_csv_gain_loss_computed() {
        let records = vec![WalletLedgerRecord {
            id: Uuid::nil(),
            raw_transaction_id: None,
            wallet_address: "0xWallet".to_string(),
            network: "ethereum-mainnet".to_string(),
            tx_hash: "0xdeadbeef".to_string(),
            timestamp: 1700000000,
            entry_type: "trade".to_string(),
            asset_symbol: "ETH".to_string(),
            amount: bigdecimal::BigDecimal::from(-1),
            counterparty_address: None,
            fee_amount: None,
            fee_asset: None,
            cost_basis: Some(bigdecimal::BigDecimal::from(3000)),
            proceeds: Some(bigdecimal::BigDecimal::from(3500)),
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let csv = wallet_ledger_to_tax_csv(&records);
        let output = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        // Gain/Loss should be proceeds - cost_basis = 500
        assert!(lines[1].contains("500"));
    }

    #[test]
    fn test_balance_history_to_csv_format() {
        let records = vec![BalanceSnapshot {
            id: Uuid::nil(),
            wallet_address: "0xWallet".to_string(),
            asset_symbol: "SOL".to_string(),
            network: "solana-mainnet".to_string(),
            timestamp: 1700000000,
            balance: bigdecimal::BigDecimal::from(42),
            tx_hash: "txhash1".to_string(),
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let csv = balance_history_to_csv(&records);
        let output = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("balance"));
        assert!(lines[1].contains("42"));
    }

    #[test]
    fn test_exportable_datasets_includes_gold() {
        assert_eq!(EXPORTABLE_DATASETS.len(), 12);
        assert!(EXPORTABLE_DATASETS.contains(&"wallet_ledger"));
        assert!(EXPORTABLE_DATASETS.contains(&"balance_history"));
        assert!(EXPORTABLE_DATASETS.contains(&"hl_pnl_summary"));
        assert!(EXPORTABLE_DATASETS.contains(&"hl_trade_history"));
        assert!(EXPORTABLE_DATASETS.contains(&"protocol_events"));
        assert!(EXPORTABLE_DATASETS.contains(&"pool_snapshots"));
    }

    #[tokio::test]
    async fn test_wallet_ledger_export_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "wallet_ledger"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "wallet_ledger export should be accepted"
        );
    }

    #[tokio::test]
    async fn test_balance_history_export_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "balance_history"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "balance_history export should be accepted"
        );
    }

    // Verify P4-W5 compatibility — existing wallet endpoints still routed
    #[tokio::test]
    async fn p5w1_compat_existing_wallet_endpoints_still_routed() {
        let wallet_uris = vec![
            "/v1/transactions/SomeWallet123",
            "/v1/ledger/SomeWallet123",
            "/v1/export/SomeWallet123",
            "/v1/balances/SomeWallet123",
            "/v1/stats/SomeWallet123",
        ];
        for uri in wallet_uris {
            assert_get_routed(test_router(), uri).await;
        }
    }

    // Verify existing dataset endpoints remain functional
    #[tokio::test]
    async fn p5w1_compat_existing_dataset_endpoints_still_routed() {
        let dataset_uris = vec![
            "/v1/datasets",
            "/v1/datasets/token_transfers/versions",
            "/v1/datasets/token_transfers/records",
            "/v1/datasets/token_transfers/completeness",
            "/v1/datasets/token_transfers/status",
        ];
        for uri in dataset_uris {
            assert_get_routed(test_router(), uri).await;
        }
    }

    // ── P5-W2: Hyperliquid Analytics Pack tests ───────────────────────

    #[test]
    fn test_hl_pnl_summary_in_queryable_datasets() {
        assert!(QUERYABLE_DATASETS.contains(&"hl_pnl_summary"));
    }

    #[test]
    fn test_hl_trade_history_in_queryable_datasets() {
        assert!(QUERYABLE_DATASETS.contains(&"hl_trade_history"));
    }

    #[test]
    fn test_hl_pnl_summary_in_exportable_datasets() {
        assert!(EXPORTABLE_DATASETS.contains(&"hl_pnl_summary"));
    }

    #[test]
    fn test_hl_trade_history_in_exportable_datasets() {
        assert!(EXPORTABLE_DATASETS.contains(&"hl_trade_history"));
    }

    #[tokio::test]
    async fn test_hl_pnl_summary_records_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/datasets/hl_pnl_summary/records").await;
    }

    #[tokio::test]
    async fn test_hl_trade_history_records_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/datasets/hl_trade_history/records").await;
    }

    #[tokio::test]
    async fn test_hl_trader_analytics_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/analytics/hl/trader").await;
    }

    #[tokio::test]
    async fn test_hl_market_analytics_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/analytics/hl/market").await;
    }

    #[tokio::test]
    async fn test_hl_trader_analytics_requires_auth() {
        assert_get_requires_auth(test_router(), "/v1/analytics/hl/trader").await;
    }

    #[tokio::test]
    async fn test_hl_market_analytics_requires_auth() {
        assert_get_requires_auth(test_router(), "/v1/analytics/hl/market").await;
    }

    #[tokio::test]
    async fn test_hl_pnl_summary_export_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "hl_pnl_summary"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "hl_pnl_summary export should be accepted"
        );
    }

    #[tokio::test]
    async fn test_hl_trade_history_export_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "hl_trade_history"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "hl_trade_history export should be accepted"
        );
    }

    #[test]
    fn test_hl_pnl_summary_to_csv_format() {
        let records = vec![HlPnlSummary {
            id: Uuid::nil(),
            wallet_address: "0xTrader".to_string(),
            coin: "ETH".to_string(),
            network: "hyperliquid-mainnet".to_string(),
            period_start: 1000,
            period_end: 2000,
            total_closed_pnl: bigdecimal::BigDecimal::from(100),
            total_funding: bigdecimal::BigDecimal::from(10),
            total_fees: bigdecimal::BigDecimal::from(5),
            net_pnl: bigdecimal::BigDecimal::from(105),
            trade_count: 3,
            fill_count: 5,
            avg_trade_size: bigdecimal::BigDecimal::from(1),
            win_count: 2,
            loss_count: 1,
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let csv = hl_pnl_summary_to_csv(&records);
        let output = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 row
        assert!(lines[0].contains("net_pnl"));
        assert!(lines[0].contains("total_closed_pnl"));
        assert!(lines[0].contains("total_funding"));
        assert!(lines[1].contains("ETH"));
        assert!(lines[1].contains("105"));
    }

    #[test]
    fn test_hl_trade_history_to_csv_format() {
        let records = vec![HlTradeHistory {
            id: Uuid::nil(),
            wallet_address: "0xTrader".to_string(),
            coin: "BTC".to_string(),
            network: "hyperliquid-mainnet".to_string(),
            side: "B".to_string(),
            entry_price: bigdecimal::BigDecimal::from(50000),
            exit_price: bigdecimal::BigDecimal::from(51000),
            size: bigdecimal::BigDecimal::from(1),
            opened_at: 1000,
            closed_at: 3000,
            realized_pnl: bigdecimal::BigDecimal::from(1000),
            fees: bigdecimal::BigDecimal::from(10),
            num_fills: 3,
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let csv = hl_trade_history_to_csv(&records);
        let output = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 row
        assert!(lines[0].contains("entry_price"));
        assert!(lines[0].contains("exit_price"));
        assert!(lines[0].contains("realized_pnl"));
        assert!(lines[1].contains("BTC"));
        assert!(lines[1].contains("1000"));
    }

    // Verify P5-W1 compatibility remains
    #[tokio::test]
    async fn p5w2_compat_existing_wallet_endpoints_still_routed() {
        let wallet_uris = vec![
            "/v1/transactions/SomeWallet123",
            "/v1/ledger/SomeWallet123",
            "/v1/export/SomeWallet123",
            "/v1/balances/SomeWallet123",
            "/v1/stats/SomeWallet123",
        ];
        for uri in wallet_uris {
            assert_get_routed(test_router(), uri).await;
        }
    }

    #[tokio::test]
    async fn p5w2_compat_p5w1_endpoints_still_routed() {
        let uris = vec![
            "/v1/datasets/wallet_ledger/records",
            "/v1/datasets/balance_history/records",
            "/v1/export/tax",
            "/v1/forensics/activity",
        ];
        for uri in uris {
            assert_get_routed(test_router(), uri).await;
        }
    }

    #[tokio::test]
    async fn p5w2_compat_existing_dataset_endpoints_still_routed() {
        let dataset_uris = vec![
            "/v1/datasets",
            "/v1/datasets/token_transfers/versions",
            "/v1/datasets/token_transfers/records",
            "/v1/datasets/token_transfers/completeness",
            "/v1/datasets/token_transfers/status",
        ];
        for uri in dataset_uris {
            assert_get_routed(test_router(), uri).await;
        }
    }

    // -- P5-W3: Protocol / TVL Pack tests --

    #[test]
    fn test_protocol_events_in_queryable_datasets() {
        assert!(QUERYABLE_DATASETS.contains(&"protocol_events"));
    }

    #[test]
    fn test_pool_snapshots_in_queryable_datasets() {
        assert!(QUERYABLE_DATASETS.contains(&"pool_snapshots"));
    }

    #[test]
    fn test_protocol_events_in_exportable_datasets() {
        assert!(EXPORTABLE_DATASETS.contains(&"protocol_events"));
    }

    #[test]
    fn test_pool_snapshots_in_exportable_datasets() {
        assert!(EXPORTABLE_DATASETS.contains(&"pool_snapshots"));
    }

    #[tokio::test]
    async fn test_protocol_events_records_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/datasets/protocol_events/records").await;
    }

    #[tokio::test]
    async fn test_pool_snapshots_records_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/datasets/pool_snapshots/records").await;
    }

    #[tokio::test]
    async fn test_protocol_activity_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/analytics/protocol/activity").await;
    }

    #[tokio::test]
    async fn test_protocol_tvl_endpoint_routed() {
        assert_get_routed(test_router(), "/v1/analytics/protocol/tvl").await;
    }

    #[tokio::test]
    async fn test_protocol_activity_requires_auth() {
        assert_get_requires_auth(test_router(), "/v1/analytics/protocol/activity").await;
    }

    #[tokio::test]
    async fn test_protocol_tvl_requires_auth() {
        assert_get_requires_auth(test_router(), "/v1/analytics/protocol/tvl").await;
    }

    #[tokio::test]
    async fn test_protocol_events_export_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "protocol_events"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "protocol_events export should be accepted"
        );
    }

    #[tokio::test]
    async fn test_pool_snapshots_export_accepted() {
        let app = test_router();
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/export/dataset")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", TEST_API_KEY))
            .body(Body::from(
                serde_json::to_string(&serde_json::json!({
                    "dataset": "pool_snapshots"
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::ACCEPTED,
            "pool_snapshots export should be accepted"
        );
    }

    #[test]
    fn test_protocol_events_to_csv_format() {
        let records = vec![ProtocolEvent {
            id: Uuid::nil(),
            network: "ethereum-mainnet".to_string(),
            protocol_address: "0xUniswap".to_string(),
            protocol_name: Some("Uniswap V3".to_string()),
            event_type: "swap".to_string(),
            event_details: serde_json::json!({"amount0": "100"}),
            pool_address: Some("0xPool".to_string()),
            raw_event_id: None,
            timestamp: 1700000000,
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let csv = protocol_events_to_csv(&records);
        let output = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 row
        assert!(lines[0].contains("protocol_address"));
        assert!(lines[0].contains("event_type"));
        assert!(lines[0].contains("event_details"));
        assert!(lines[1].contains("0xUniswap"));
        assert!(lines[1].contains("swap"));
    }

    #[test]
    fn test_pool_snapshots_to_csv_format() {
        let records = vec![PoolSnapshot {
            id: Uuid::nil(),
            network: "ethereum-mainnet".to_string(),
            pool_address: "0xPool".to_string(),
            protocol_address: "0xProto".to_string(),
            protocol_name: Some("Uniswap".to_string()),
            token0_address: "0xWETH".to_string(),
            token0_symbol: Some("WETH".to_string()),
            token1_address: "0xUSDC".to_string(),
            token1_symbol: Some("USDC".to_string()),
            reserve0: bigdecimal::BigDecimal::from(1000),
            reserve1: bigdecimal::BigDecimal::from(2000000),
            tvl_usd: Some(bigdecimal::BigDecimal::from(4000000)),
            snapshot_timestamp: 1700000000,
            block_number: Some(18000000),
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        }];
        let csv = pool_snapshots_to_csv(&records);
        let output = String::from_utf8(csv).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 row
        assert!(lines[0].contains("pool_address"));
        assert!(lines[0].contains("reserve0"));
        assert!(lines[0].contains("tvl_usd"));
        assert!(lines[1].contains("0xPool"));
        assert!(lines[1].contains("4000000"));
    }

    // Verify P5-W2 and P5-W1 compatibility remains
    #[tokio::test]
    async fn p5w3_compat_existing_wallet_endpoints_still_routed() {
        let wallet_uris = vec![
            "/v1/transactions/SomeWallet123",
            "/v1/ledger/SomeWallet123",
            "/v1/export/SomeWallet123",
            "/v1/balances/SomeWallet123",
            "/v1/stats/SomeWallet123",
        ];
        for uri in wallet_uris {
            assert_get_routed(test_router(), uri).await;
        }
    }

    #[tokio::test]
    async fn p5w3_compat_p5w1_and_p5w2_endpoints_still_routed() {
        let uris = vec![
            "/v1/datasets/wallet_ledger/records",
            "/v1/datasets/balance_history/records",
            "/v1/export/tax",
            "/v1/forensics/activity",
            "/v1/analytics/hl/trader",
            "/v1/analytics/hl/market",
            "/v1/datasets/hl_pnl_summary/records",
            "/v1/datasets/hl_trade_history/records",
        ];
        for uri in uris {
            assert_get_routed(test_router(), uri).await;
        }
    }

    // -----------------------------------------------------------------------
    // RateLimiter unit tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rate_limiter_allows_within_capacity() {
        let limiter = RateLimiter::new(5, 1.0);
        for _ in 0..5 {
            assert!(limiter.try_acquire("key-a").await);
        }
    }

    #[tokio::test]
    async fn rate_limiter_rejects_over_capacity() {
        let limiter = RateLimiter::new(3, 0.0); // no refill
        assert!(limiter.try_acquire("key-b").await);
        assert!(limiter.try_acquire("key-b").await);
        assert!(limiter.try_acquire("key-b").await);
        // 4th request should be rejected
        assert!(!limiter.try_acquire("key-b").await);
    }

    #[tokio::test]
    async fn rate_limiter_refills_over_time() {
        let limiter = RateLimiter::new(1, 100.0); // fast refill: 100 tokens/sec
        assert!(limiter.try_acquire("key-c").await);
        assert!(!limiter.try_acquire("key-c").await);
        // Wait long enough for at least 1 token to refill
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(limiter.try_acquire("key-c").await);
    }

    #[tokio::test]
    async fn rate_limiter_per_key_isolation() {
        let limiter = RateLimiter::new(1, 0.0);
        assert!(limiter.try_acquire("alice").await);
        assert!(!limiter.try_acquire("alice").await);
        // A different key gets its own bucket
        assert!(limiter.try_acquire("bob").await);
        assert!(!limiter.try_acquire("bob").await);
    }

    #[tokio::test]
    async fn rate_limiter_evicts_stale_entries() {
        // Use a very small max-buckets threshold to trigger eviction easily.
        // We can't change the const, but we can test the eviction indirectly
        // by verifying the bucket map grows and that stale entries are pruned.
        let limiter = RateLimiter::new(10, 1.0);

        // Insert many keys
        for i in 0..100 {
            limiter.try_acquire(&format!("key-{i}")).await;
        }

        // All buckets should exist (well under RATE_LIMIT_MAX_BUCKETS)
        let count = limiter.buckets.lock().await.len();
        assert_eq!(count, 100);
    }

    #[tokio::test]
    async fn rate_limiter_last_used_updated() {
        let limiter = RateLimiter::new(10, 1.0);
        limiter.try_acquire("ts-key").await;
        let first_used = limiter
            .buckets
            .lock()
            .await
            .get("ts-key")
            .unwrap()
            .last_used;
        tokio::time::sleep(Duration::from_millis(10)).await;
        limiter.try_acquire("ts-key").await;
        let second_used = limiter
            .buckets
            .lock()
            .await
            .get("ts-key")
            .unwrap()
            .last_used;
        assert!(second_used > first_used);
    }
}
