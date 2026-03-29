//! Durable stream subscription orchestrator.
//!
//! Polls `stream_subscriptions` for claimable rows (desired_status = 'active'
//! with no lease or stale heartbeat), claims them, and spawns per-subscription
//! stream tasks. Each task heartbeats its lease and tears down when the
//! desired_status changes or the lease is lost.
//!
//! This module mirrors the ingestion worker (`worker.rs`) and export worker
//! (`export_worker.rs`) patterns: durable intent in Postgres, process-local
//! execution with heartbeat-based liveness.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use spectraplex_adapters::hyperliquid_ws::HyperliquidWsClient;
use spectraplex_adapters::repo::Repository;
use spectraplex_adapters::solana_grpc::SolanaGrpcAdapter;
use spectraplex_core::config::AppConfig;
use spectraplex_core::models::{Chain, Transaction};
use spectraplex_core::provider::{NetworkContext, NetworkId, ProviderCapability, ProviderRegistry};
use spectraplex_core::v2::{StreamSource, StreamSubscription};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// How often the orchestrator polls for new claimable subscriptions.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How often each stream task heartbeats its lease.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// How often the orchestrator checks desired_status for running streams.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// Maximum number of subscriptions to claim per poll cycle.
const MAX_CLAIM_BATCH: i64 = 10;

/// Flush batch every N transactions.
const BATCH_SIZE: usize = 100;

/// Flush batch every N seconds.
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// State for a locally-running stream task.
struct ActiveStream {
    cancel: CancellationToken,
}

/// Spawn the stream subscription orchestrator as a background task.
///
/// The orchestrator:
/// 1. Polls for claimable subscriptions (active + unclaimed/stale)
/// 2. Claims leases and spawns stream tasks
/// 3. Periodically reconciles: stops tasks whose desired_status changed
/// 4. On restart, re-claims stale subscriptions from dead workers
pub fn spawn_stream_orchestrator(
    repo: Repository,
    config: Arc<AppConfig>,
    provider_registry: Arc<ProviderRegistry>,
    stream_semaphore: Arc<Semaphore>,
    worker_id: String,
) {
    tokio::spawn(async move {
        info!(worker_id = %worker_id, "Stream orchestrator started");

        let mut active_streams: HashMap<Uuid, ActiveStream> = HashMap::new();
        let mut poll_interval = tokio::time::interval(POLL_INTERVAL);
        let mut reconcile_interval = tokio::time::interval(RECONCILE_INTERVAL);

        loop {
            tokio::select! {
                _ = poll_interval.tick() => {
                    poll_and_claim(
                        &repo,
                        &config,
                        &provider_registry,
                        &stream_semaphore,
                        &worker_id,
                        &mut active_streams,
                    ).await;
                }
                _ = reconcile_interval.tick() => {
                    reconcile(&repo, &worker_id, &mut active_streams).await;
                }
            }
        }
    });
}

/// Poll for claimable subscriptions and attempt to claim them.
async fn poll_and_claim(
    repo: &Repository,
    config: &Arc<AppConfig>,
    provider_registry: &Arc<ProviderRegistry>,
    stream_semaphore: &Arc<Semaphore>,
    worker_id: &str,
    active_streams: &mut HashMap<Uuid, ActiveStream>,
) {
    let claimable = match repo
        .list_claimable_stream_subscriptions(MAX_CLAIM_BATCH)
        .await
    {
        Ok(subs) => subs,
        Err(e) => {
            debug!(error = %e, "Failed to list claimable stream subscriptions");
            return;
        }
    };

    for sub in claimable {
        // Skip if we're already running this subscription locally.
        if active_streams.contains_key(&sub.id) {
            continue;
        }

        // Acquire a semaphore permit for local concurrency limiting.
        let permit = match stream_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                debug!("Stream semaphore full, skipping claim cycle");
                return;
            }
        };

        match repo.claim_stream_lease(sub.id, worker_id).await {
            Ok(true) => {
                info!(
                    subscription_id = %sub.id,
                    network = %sub.network,
                    source = %sub.source,
                    "Claimed stream subscription lease"
                );

                let cancel = CancellationToken::new();
                active_streams.insert(
                    sub.id,
                    ActiveStream {
                        cancel: cancel.clone(),
                    },
                );

                spawn_stream_task(
                    repo.clone(),
                    config.clone(),
                    provider_registry.clone(),
                    sub,
                    cancel,
                    worker_id.to_string(),
                    permit,
                );
            }
            Ok(false) => {
                // Someone else claimed it first — race is expected.
                debug!(subscription_id = %sub.id, "Stream subscription already claimed by another worker");
                drop(permit);
            }
            Err(e) => {
                error!(subscription_id = %sub.id, error = %e, "Failed to claim stream lease");
                drop(permit);
            }
        }
    }
}

/// Spawn a stream task that runs the appropriate adapter, heartbeats, and
/// flushes transaction batches.
fn spawn_stream_task(
    repo: Repository,
    config: Arc<AppConfig>,
    provider_registry: Arc<ProviderRegistry>,
    sub: StreamSubscription,
    cancel: CancellationToken,
    worker_id: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let sub_id = sub.id;
    tokio::spawn(async move {
        let _permit = _permit; // hold permit until task ends

        let result = match sub.source {
            StreamSource::Grpc => {
                run_solana_grpc_stream(
                    &repo,
                    &config,
                    &provider_registry,
                    &sub,
                    &cancel,
                    &worker_id,
                )
                .await
            }
            StreamSource::Ws => {
                run_hyperliquid_ws_stream(&repo, &provider_registry, &sub, &cancel, &worker_id)
                    .await
            }
            StreamSource::Rpc => {
                // RPC streaming not yet implemented
                Err(anyhow::anyhow!("RPC stream source not yet implemented"))
            }
        };

        if let Err(e) = result {
            error!(subscription_id = %sub_id, error = %e, "Stream task failed");
            if let Err(fail_err) = repo
                .fail_stream_subscription(sub_id, &worker_id, &e.to_string())
                .await
            {
                error!(subscription_id = %sub_id, error = %fail_err, "Failed to mark subscription as errored");
            }
        } else {
            // Clean exit (cancelled) — release the lease
            if let Err(e) = repo.release_stream_lease(sub_id, &worker_id).await {
                error!(subscription_id = %sub_id, error = %e, "Failed to release stream lease");
            }
        }

        info!(subscription_id = %sub_id, "Stream task ended");
    });
}

/// Run a Solana gRPC stream for a subscription.
async fn run_solana_grpc_stream(
    repo: &Repository,
    config: &Arc<AppConfig>,
    provider_registry: &Arc<ProviderRegistry>,
    sub: &StreamSubscription,
    cancel: &CancellationToken,
    worker_id: &str,
) -> anyhow::Result<()> {
    let net_ctx =
        NetworkContext::from_registry(provider_registry, &NetworkId::new(sub.network.clone()));

    let adapter = match &net_ctx {
        Some(ctx) if ctx.has_capability(ProviderCapability::Stream) => {
            SolanaGrpcAdapter::from_network_context(ctx)?
        }
        _ => {
            let grpc_url = config
                .solana_grpc_url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("No Solana gRPC URL configured"))?;
            let grpc_token = config.solana_grpc_token.clone();
            SolanaGrpcAdapter::new(grpc_url, grpc_token)
        }
    };

    let (mut rx, grpc_handle) = adapter.stream_transactions();
    let sub_id = sub.id;

    let mut batch: Vec<Transaction> = Vec::new();
    let mut last_flush = tokio::time::Instant::now();
    let mut tx_count: u64 = cursor_tx_count(sub);
    let mut last_slot: u64 = cursor_last_slot(sub);
    let mut hb = tokio::time::interval(HEARTBEAT_INTERVAL);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(subscription_id = %sub_id, "Solana stream cancelled");
                break;
            }
            _ = hb.tick() => {
                match repo.heartbeat_stream(sub_id, worker_id).await {
                    Ok(true) => {
                        debug!(subscription_id = %sub_id, "Stream heartbeat OK");
                    }
                    Ok(false) => {
                        warn!(subscription_id = %sub_id, "Stream lease lost (heartbeat rejected)");
                        grpc_handle.abort();
                        return Ok(());
                    }
                    Err(e) => {
                        error!(subscription_id = %sub_id, error = %e, "Stream heartbeat error");
                    }
                }
            }
            maybe_tx = rx.recv() => {
                match maybe_tx {
                    Some(tx) => {
                        if let Some(slot) = tx.raw_metadata.get("slot").and_then(|v| v.as_u64()) {
                            last_slot = slot;
                        }
                        batch.push(tx);
                        tx_count += 1;

                        if batch.len() >= BATCH_SIZE || last_flush.elapsed() >= FLUSH_INTERVAL {
                            if let Err(e) = repo.save_transactions(&batch).await {
                                error!(subscription_id = %sub_id, error = %e, "Failed to save streamed transactions");
                            }
                            batch.clear();
                            last_flush = tokio::time::Instant::now();

                            // Update cursor state periodically
                            let cursor = serde_json::json!({
                                "tx_count": tx_count,
                                "last_slot": last_slot,
                            });
                            if let Err(e) = repo.update_stream_cursor(sub_id, &cursor).await {
                                warn!(subscription_id = %sub_id, error = %e, "Failed to update cursor");
                            }
                        }
                    }
                    None => {
                        info!(subscription_id = %sub_id, "gRPC stream channel closed");
                        break;
                    }
                }
            }
        }
    }

    // Flush remaining batch
    if !batch.is_empty() {
        if let Err(e) = repo.save_transactions(&batch).await {
            error!(subscription_id = %sub_id, error = %e, "Failed to flush final batch");
        }
        let cursor = serde_json::json!({
            "tx_count": tx_count,
            "last_slot": last_slot,
        });
        let _ = repo.update_stream_cursor(sub_id, &cursor).await;
    }

    grpc_handle.abort();
    Ok(())
}

/// Run a Hyperliquid WebSocket stream for a subscription.
async fn run_hyperliquid_ws_stream(
    repo: &Repository,
    provider_registry: &Arc<ProviderRegistry>,
    sub: &StreamSubscription,
    cancel: &CancellationToken,
    worker_id: &str,
) -> anyhow::Result<()> {
    let wallet = sub
        .config
        .as_ref()
        .and_then(|c| c.get("wallet").and_then(|w| w.as_str()))
        .ok_or_else(|| anyhow::anyhow!("Missing wallet in subscription config"))?
        .to_string();

    let net_ctx =
        NetworkContext::from_registry(provider_registry, &NetworkId::new(sub.network.clone()));

    let client = match &net_ctx {
        Some(ctx) => HyperliquidWsClient::from_network_context(ctx),
        None => HyperliquidWsClient::new(),
    };

    let sub_id = sub.id;
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<serde_json::Value>(1000);

    // Spawn WS connection task with retry logic
    let ws_cancel = cancel.clone();
    let ws_wallet = wallet.clone();
    let ws_handle = tokio::spawn(async move {
        let mut retry_count: u32 = 0;
        const MAX_RETRIES: u32 = 10;
        loop {
            let ws_wallet_ref = ws_wallet.clone();
            let sender_ref = sender.clone();
            tokio::select! {
                result = client.subscribe_user(&ws_wallet_ref, |msg| {
                    let channel = msg.channel.as_deref().unwrap_or("");
                    if channel == "subscriptionResponse" || channel.is_empty() {
                        return;
                    }
                    if let Some(data) = msg.data {
                        if let Err(tokio::sync::mpsc::error::TrySendError::Full(_)) = sender_ref.try_send(data) {
                            warn!(subscription_id = %sub_id, "Hyperliquid WS channel full, message dropped");
                        }
                    }
                }) => {
                    if let Err(e) = result {
                        error!(subscription_id = %sub_id, error = %e, "Hyperliquid WebSocket error");
                    }
                    if ws_cancel.is_cancelled() {
                        break;
                    }
                    retry_count += 1;
                    if retry_count > MAX_RETRIES {
                        error!(subscription_id = %sub_id, "Exceeded max retries ({MAX_RETRIES}), stopping Hyperliquid WS stream");
                        break;
                    }
                    let backoff = Duration::from_secs(2u64.saturating_pow(retry_count.min(6)));
                    warn!(subscription_id = %sub_id, retry = retry_count, max = MAX_RETRIES,
                        "Hyperliquid WebSocket disconnected, reconnecting in {:?}", backoff);
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = ws_cancel.cancelled() => { break; }
                    }
                }
                _ = ws_cancel.cancelled() => {
                    info!(subscription_id = %sub_id, "Hyperliquid stream cancelled");
                    break;
                }
            }
        }
    });

    let user_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, wallet.as_bytes());
    let mut batch: Vec<Transaction> = Vec::new();
    let mut last_flush = tokio::time::Instant::now();
    let mut tx_count: u64 = cursor_tx_count(sub);
    let mut hb = tokio::time::interval(HEARTBEAT_INTERVAL);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(subscription_id = %sub_id, "Hyperliquid stream cancelled");
                break;
            }
            _ = hb.tick() => {
                match repo.heartbeat_stream(sub_id, worker_id).await {
                    Ok(true) => {
                        debug!(subscription_id = %sub_id, "Stream heartbeat OK");
                    }
                    Ok(false) => {
                        warn!(subscription_id = %sub_id, "Stream lease lost (heartbeat rejected)");
                        ws_handle.abort();
                        return Ok(());
                    }
                    Err(e) => {
                        error!(subscription_id = %sub_id, error = %e, "Stream heartbeat error");
                    }
                }
            }
            msg = receiver.recv() => {
                match msg {
                    Some(data) => {
                        let tx = Transaction {
                            id: Uuid::new_v4(),
                            user_id,
                            wallet_address: wallet.clone(),
                            timestamp: chrono::Utc::now().timestamp(),
                            tx_hash: format!("hl-ws-{}", Uuid::new_v4()),
                            chain: Chain::Hyperliquid,
                            raw_metadata: data,
                        };
                        batch.push(tx);
                        tx_count += 1;

                        if batch.len() >= BATCH_SIZE || last_flush.elapsed() >= FLUSH_INTERVAL {
                            if let Err(e) = repo.save_transactions(&batch).await {
                                error!(subscription_id = %sub_id, error = %e, "Failed to save HL stream batch");
                            }
                            batch.clear();
                            last_flush = tokio::time::Instant::now();

                            let cursor = serde_json::json!({
                                "tx_count": tx_count,
                            });
                            if let Err(e) = repo.update_stream_cursor(sub_id, &cursor).await {
                                warn!(subscription_id = %sub_id, error = %e, "Failed to update cursor");
                            }
                        }
                    }
                    None => {
                        info!(subscription_id = %sub_id, "Hyperliquid WS channel closed");
                        break;
                    }
                }
            }
        }
    }

    // Flush remaining batch
    if !batch.is_empty() {
        if let Err(e) = repo.save_transactions(&batch).await {
            error!(subscription_id = %sub_id, error = %e, "Failed to flush final HL batch");
        }
        let cursor = serde_json::json!({ "tx_count": tx_count });
        let _ = repo.update_stream_cursor(sub_id, &cursor).await;
    }

    ws_handle.abort();
    Ok(())
}

/// Extract tx_count from cursor_state JSON, default 0.
fn cursor_tx_count(sub: &StreamSubscription) -> u64 {
    sub.cursor_state
        .as_ref()
        .and_then(|c| c.get("tx_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// Extract last_slot from cursor_state JSON, default 0.
fn cursor_last_slot(sub: &StreamSubscription) -> u64 {
    sub.cursor_state
        .as_ref()
        .and_then(|c| c.get("last_slot"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// Reconcile locally-running streams against durable state.
///
/// - If desired_status changed to 'stopped', cancel the task and release the lease.
/// - If the subscription no longer exists, cancel the task.
/// - Remove entries for tasks that have already been cancelled.
async fn reconcile(
    repo: &Repository,
    worker_id: &str,
    active_streams: &mut HashMap<Uuid, ActiveStream>,
) {
    let sub_ids: Vec<Uuid> = active_streams.keys().copied().collect();

    for sub_id in sub_ids {
        let sub = match repo.get_stream_subscription(sub_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                // Subscription deleted — cancel and remove.
                if let Some(stream) = active_streams.remove(&sub_id) {
                    warn!(subscription_id = %sub_id, "Subscription deleted, cancelling stream");
                    stream.cancel.cancel();
                }
                continue;
            }
            Err(e) => {
                error!(subscription_id = %sub_id, error = %e, "Failed to fetch subscription for reconcile");
                continue;
            }
        };

        // If desired_status is no longer 'active', stop the stream.
        if sub.desired_status != spectraplex_core::v2::StreamDesiredStatus::Active {
            if let Some(stream) = active_streams.remove(&sub_id) {
                info!(
                    subscription_id = %sub_id,
                    desired_status = %sub.desired_status,
                    "Desired status changed, stopping stream"
                );
                stream.cancel.cancel();
                let _ = repo.release_stream_lease(sub_id, worker_id).await;
            }
            continue;
        }

        // If lease_owner is no longer us, someone reclaimed it.
        if sub.lease_owner.as_deref() != Some(worker_id) {
            if let Some(stream) = active_streams.remove(&sub_id) {
                warn!(
                    subscription_id = %sub_id,
                    current_owner = ?sub.lease_owner,
                    "Lease reclaimed by another worker, cancelling local stream"
                );
                stream.cancel.cancel();
            }
        }
    }
}
