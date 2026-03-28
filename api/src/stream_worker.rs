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

use spectraplex_adapters::repo::Repository;
use spectraplex_core::config::AppConfig;
use spectraplex_core::provider::ProviderRegistry;
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
    _config: Arc<AppConfig>,
    _provider_registry: Arc<ProviderRegistry>,
    _stream_semaphore: Arc<Semaphore>,
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
                    poll_and_claim(&repo, &worker_id, &mut active_streams).await;
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
    worker_id: &str,
    active_streams: &mut HashMap<Uuid, ActiveStream>,
) {
    let claimable = match repo.list_claimable_stream_subscriptions(MAX_CLAIM_BATCH).await {
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

        match repo.claim_stream_lease(sub.id, worker_id).await {
            Ok(true) => {
                info!(
                    subscription_id = %sub.id,
                    network = %sub.network,
                    source = %sub.source,
                    "Claimed stream subscription lease"
                );

                let cancel = CancellationToken::new();
                active_streams.insert(sub.id, ActiveStream {
                    cancel: cancel.clone(),
                });

                // TODO(task-2): Spawn the actual stream task here.
                // For now, just start a heartbeat-only placeholder task.
                let repo_clone = repo.clone();
                let wid = worker_id.to_string();
                let sub_id = sub.id;
                let cancel_clone = cancel.clone();
                tokio::spawn(async move {
                    let mut hb = tokio::time::interval(HEARTBEAT_INTERVAL);
                    loop {
                        tokio::select! {
                            _ = cancel_clone.cancelled() => {
                                info!(subscription_id = %sub_id, "Stream task cancelled");
                                break;
                            }
                            _ = hb.tick() => {
                                match repo_clone.heartbeat_stream(sub_id, &wid).await {
                                    Ok(true) => {
                                        debug!(subscription_id = %sub_id, "Stream heartbeat OK");
                                    }
                                    Ok(false) => {
                                        warn!(subscription_id = %sub_id, "Stream lease lost (heartbeat rejected)");
                                        break;
                                    }
                                    Err(e) => {
                                        error!(subscription_id = %sub_id, error = %e, "Stream heartbeat error");
                                    }
                                }
                            }
                        }
                    }
                });
            }
            Ok(false) => {
                // Someone else claimed it first — race is expected.
                debug!(subscription_id = %sub.id, "Stream subscription already claimed by another worker");
            }
            Err(e) => {
                error!(subscription_id = %sub.id, error = %e, "Failed to claim stream lease");
            }
        }
    }
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
