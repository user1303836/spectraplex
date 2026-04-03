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

use std::collections::HashSet;

use spectraplex_adapters::dual_write::{build_target_matches, v1_tx_to_v2_raw};
use spectraplex_adapters::hyperliquid_ws::HyperliquidWsClient;
use spectraplex_adapters::repo::Repository;
use spectraplex_adapters::solana_grpc::SolanaGrpcAdapter;
use spectraplex_core::config::AppConfig;
use spectraplex_core::models::{Chain, Transaction};
use spectraplex_core::provider::{NetworkContext, NetworkId, ProviderCapability, ProviderRegistry};
use spectraplex_core::v2::{
    Checkpoint, IngestionRun, RawTransaction, StreamSource, StreamSubscription,
};
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
                            let cursor = serde_json::json!({
                                "tx_count": tx_count,
                                "last_slot": last_slot,
                            });
                            if flush_stream_batch(repo, &batch, &sub.network, "grpc", sub.target_id, sub_id, Some(&cursor)).await {
                                batch.clear();
                                last_flush = tokio::time::Instant::now();
                                if let Err(e) = repo.update_stream_cursor(sub_id, &cursor).await {
                                    warn!(subscription_id = %sub_id, error = %e, "Failed to update cursor");
                                }
                            }
                        }
                    }
                    None => {
                        // Channel closed = gRPC task ended.  This is NOT a clean
                        // cancellation — the adapter died or the remote closed the
                        // connection.  Return an error so the worker marks the
                        // subscription as failed instead of silently recycling it.
                        warn!(subscription_id = %sub_id, "gRPC stream channel closed unexpectedly");

                        // Flush remaining batch before failing
                        if !batch.is_empty() {
                            let cursor = serde_json::json!({
                                "tx_count": tx_count,
                                "last_slot": last_slot,
                            });
                            flush_stream_batch(repo, &batch, &sub.network, "grpc", sub.target_id, sub_id, Some(&cursor)).await;
                            let _ = repo.update_stream_cursor(sub_id, &cursor).await;
                        }
                        grpc_handle.abort();
                        return Err(anyhow::anyhow!("gRPC stream channel closed unexpectedly"));
                    }
                }
            }
        }
    }

    // Flush remaining batch
    if !batch.is_empty() {
        let cursor = serde_json::json!({
            "tx_count": tx_count,
            "last_slot": last_slot,
        });
        flush_stream_batch(
            repo,
            &batch,
            &sub.network,
            "grpc",
            sub.target_id,
            sub_id,
            Some(&cursor),
        )
        .await;
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
                            let cursor = serde_json::json!({
                                "tx_count": tx_count,
                            });
                            if flush_stream_batch(repo, &batch, &sub.network, "ws", sub.target_id, sub_id, Some(&cursor)).await {
                                batch.clear();
                                last_flush = tokio::time::Instant::now();
                                if let Err(e) = repo.update_stream_cursor(sub_id, &cursor).await {
                                    warn!(subscription_id = %sub_id, error = %e, "Failed to update cursor");
                                }
                            }
                        }
                    }
                    None => {
                        // Channel closed = WS task ended (retries exhausted or
                        // fatal error).  NOT a clean cancellation — fail the
                        // subscription so it doesn't silently recycle.
                        warn!(subscription_id = %sub_id, "Hyperliquid WS channel closed (retries exhausted)");

                        // Flush remaining batch before failing
                        if !batch.is_empty() {
                            let cursor = serde_json::json!({ "tx_count": tx_count });
                            flush_stream_batch(repo, &batch, &sub.network, "ws", sub.target_id, sub_id, Some(&cursor)).await;
                            let _ = repo.update_stream_cursor(sub_id, &cursor).await;
                        }
                        ws_handle.abort();
                        return Err(anyhow::anyhow!("Hyperliquid WS stream closed (retries exhausted)"));
                    }
                }
            }
        }
    }

    // Flush remaining batch
    if !batch.is_empty() {
        let cursor = serde_json::json!({ "tx_count": tx_count });
        flush_stream_batch(
            repo,
            &batch,
            &sub.network,
            "ws",
            sub.target_id,
            sub_id,
            Some(&cursor),
        )
        .await;
        let _ = repo.update_stream_cursor(sub_id, &cursor).await;
    }

    ws_handle.abort();
    Ok(())
}

/// Flush a batch of V1 transactions using the V2-first dual-write pattern
/// with full Bronze lineage (ingestion run, checkpoint, materialization handoff).
///
/// 1. Create an `IngestionRun` for this flush batch (mode="stream")
/// 2. Convert V1 → V2 `RawTransaction`, stamp `ingestion_run_id`, dedup
/// 3. Upsert into `raw_transactions` (authoritative V2 write)
/// 4. Write `target_matches` if a target_id is present
/// 5. Upsert V2 checkpoint from cursor state
/// 6. Complete the ingestion run
/// 7. Auto-trigger downstream materialization
/// 8. Best-effort V1 compat via `save_transactions`
///
/// Returns `true` if the V2 upsert succeeded (callers should clear the batch),
/// `false` if it failed (callers should retain the batch for retry).
async fn flush_stream_batch(
    repo: &Repository,
    batch: &[Transaction],
    network: &str,
    source: &str,
    target_id: Option<Uuid>,
    sub_id: Uuid,
    cursor_state: Option<&serde_json::Value>,
) -> bool {
    if batch.is_empty() {
        return true;
    }

    // Create an ingestion run for this flush batch.
    let run_id = Uuid::new_v4();
    let run = IngestionRun {
        id: run_id,
        target_id,
        network: network.to_string(),
        source: source.to_string(),
        mode: "stream".to_string(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        finished_at: None,
        records_written: 0,
        error_message: None,
        cursor_state: cursor_state.cloned(),
    };
    let run_created = repo.create_ingestion_run(&run).await.is_ok();
    if !run_created {
        warn!(
            subscription_id = %sub_id,
            "Failed to create stream ingestion run; raw_transactions will not carry ingestion_run_id"
        );
    }

    // V2 authoritative
    let v2_batch: Vec<RawTransaction> = batch
        .iter()
        .map(|tx| {
            let mut raw = v1_tx_to_v2_raw(tx, Some(network));
            raw.source = source.to_string();
            if run_created {
                raw.ingestion_run_id = Some(run_id);
            }
            raw
        })
        .collect();
    let mut seen = HashSet::new();
    let v2_deduped: Vec<RawTransaction> = v2_batch
        .into_iter()
        .filter(|r| seen.insert((r.network.clone(), r.tx_hash.clone())))
        .collect();

    let record_count = v2_deduped.len() as i64;

    match repo
        .upsert_raw_transactions_returning_ids(&v2_deduped)
        .await
    {
        Ok(canonical_ids) => {
            if let Some(tid) = target_id {
                let matches = build_target_matches(tid, &canonical_ids);
                if let Err(e) = repo.save_target_matches(&matches).await {
                    error!(
                        subscription_id = %sub_id,
                        error = %e,
                        "Failed to save target matches"
                    );
                }

                // Upsert V2 checkpoint from stream cursor state. This records
                // stream progress in the V2 checkpoint table under the stream's
                // source (grpc/ws). Backfill uses different sources (rpc/rest)
                // with different cursor schemas, so these checkpoints track
                // stream-specific progress rather than enabling cross-source resume.
                if let Some(cursor) = cursor_state {
                    let v2_cp = Checkpoint {
                        id: Uuid::new_v5(
                            &Uuid::NAMESPACE_URL,
                            format!("{}:{}:{}", tid, network, source).as_bytes(),
                        ),
                        target_id: tid,
                        network: network.to_string(),
                        source: source.to_string(),
                        cursor: cursor.clone(),
                        updated_at: chrono::Utc::now(),
                    };
                    if let Err(e) = repo.upsert_checkpoint_v2(&v2_cp).await {
                        warn!(
                            subscription_id = %sub_id,
                            error = %e,
                            "Failed to upsert V2 checkpoint (non-fatal)"
                        );
                    }
                }
            }

            // Complete the ingestion run.
            if run_created {
                let _ = repo
                    .update_ingestion_run_status(
                        run_id,
                        "completed",
                        Some(chrono::Utc::now()),
                        record_count,
                        None,
                    )
                    .await;

                // Auto-trigger materialization for wallet-type targets only.
                // Global/program streams (e.g. __global_grpc_stream__) do not
                // have a meaningful wallet address and should not trigger
                // wallet-scoped normalize runs.
                if let Some(tid) = target_id {
                    match repo.get_index_target(tid).await {
                        Ok(Some(target)) => {
                            let wallet = target.address.unwrap_or_default();
                            if !wallet.is_empty()
                                && target.kind == spectraplex_core::v2::TargetKind::Wallet
                            {
                                let mat_scope = serde_json::json!({
                                    "wallet": wallet,
                                    "network": network,
                                    "ingestion_run_id": run_id.to_string(),
                                });
                                if let Err(e) = repo
                                    .create_materialization_run(
                                        "normalize",
                                        Some(&mat_scope),
                                        None,
                                        None,
                                        None,
                                    )
                                    .await
                                {
                                    warn!(subscription_id = %sub_id, error = %e, "Failed to enqueue post-stream materialization (non-fatal)");
                                } else {
                                    info!(subscription_id = %sub_id, run_id = %run_id, "Enqueued post-stream materialization run");
                                }
                            }
                        }
                        Ok(None) => {
                            warn!(subscription_id = %sub_id, target_id = %tid, "Index target not found for post-stream materialization");
                        }
                        Err(e) => {
                            warn!(subscription_id = %sub_id, error = %e, "Failed to fetch index target for post-stream materialization");
                        }
                    }
                }
            }
        }
        Err(e) => {
            error!(
                subscription_id = %sub_id,
                error = %e,
                "Failed to upsert V2 raw_transactions"
            );

            // Fail the ingestion run if V2 write failed.
            if run_created {
                let _ = repo
                    .update_ingestion_run_status(
                        run_id,
                        "failed",
                        Some(chrono::Utc::now()),
                        0,
                        Some(&e.to_string()),
                    )
                    .await;
            }

            // V1 compat (best-effort) — still attempt even on V2 failure
            if let Err(e) = repo.save_transactions(batch).await {
                warn!(
                    subscription_id = %sub_id,
                    error = %e,
                    "V1 compat: save_transactions failed (non-fatal)"
                );
            }

            return false;
        }
    }

    // V1 compat (best-effort)
    if let Err(e) = repo.save_transactions(batch).await {
        warn!(
            subscription_id = %sub_id,
            error = %e,
            "V1 compat: save_transactions failed (non-fatal)"
        );
    }

    true
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
