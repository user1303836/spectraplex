//! In-process ingestion worker loop.
//!
//! Claims pending `ingestion_jobs` rows from Postgres, executes the actual
//! chain fetch via the existing adapter layer, and marks jobs complete/failed
//! durably. Heartbeats while running so that dead workers are detected and
//! their jobs reclaimed.

use std::time::Duration;

use spectraplex_adapters::dual_write::{
    build_target_matches, chain_to_default_source, v1_checkpoint_to_v2_with_network,
    v1_tx_to_v2_raw,
};
use spectraplex_adapters::evm::EvmAdapter;
use spectraplex_adapters::hyperliquid::HyperliquidAdapter;
use spectraplex_adapters::repo::{build_checkpoint, Repository};
use spectraplex_adapters::solana::SolanaAdapter;
use spectraplex_core::config::AppConfig;
use spectraplex_core::connector::Connector;
use spectraplex_core::models::{Chain, ChainIngestor, Transaction};
use spectraplex_core::provider::{NetworkContext, NetworkId, ProviderRegistry};
use spectraplex_core::v2::{IndexTarget, IngestionBatch, IngestionJob, RawTransaction, TargetKind};
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

/// How often the worker polls for new jobs when idle.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How often the worker sends heartbeats while executing a job.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the ingestion worker loop. Returns immediately; the worker runs
/// until `cancel` is triggered.
pub fn spawn_ingestion_worker(
    repo: Repository,
    config: AppConfig,
    provider_registry: ProviderRegistry,
    cancel: CancellationToken,
) {
    let worker_id = format!("worker-{}", Uuid::new_v4());
    info!(worker_id = %worker_id, "Starting ingestion worker");

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(worker_id = %worker_id, "Ingestion worker shutting down");
                    return;
                }
                _ = worker_tick(&repo, &config, &provider_registry, &worker_id) => {}
            }
        }
    });
}

async fn worker_tick(
    repo: &Repository,
    config: &AppConfig,
    provider_registry: &ProviderRegistry,
    worker_id: &str,
) {
    match repo.claim_ingestion_job(worker_id).await {
        Ok(Some(job)) => {
            info!(
                job_id = %job.id,
                network = %job.network,
                worker_id = %worker_id,
                "Claimed ingestion job"
            );
            execute_job(repo, config, provider_registry, worker_id, &job).await;
        }
        Ok(None) => {
            // No jobs available -- sleep before polling again.
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Err(e) => {
            error!(error = %e, worker_id = %worker_id, "Failed to claim ingestion job");
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

async fn execute_job(
    repo: &Repository,
    config: &AppConfig,
    provider_registry: &ProviderRegistry,
    worker_id: &str,
    job: &IngestionJob,
) {
    let job_id = job.id;

    // Transition claimed -> running.
    if let Err(e) = repo.mark_ingestion_job_running(job_id, worker_id).await {
        error!(job_id = %job_id, error = %e, "Failed to mark job running");
        let _ = repo
            .fail_ingestion_job(job_id, worker_id, &format!("failed to mark running: {e}"))
            .await;
        return;
    }

    // Spawn a heartbeat task that runs until we signal completion.
    // If the heartbeat detects the lease was reclaimed (Ok(false)),
    // it cancels lease_lost so the main task can abort.
    let heartbeat_cancel = CancellationToken::new();
    let lease_lost = CancellationToken::new();
    let hb_repo = repo.clone();
    let hb_worker = worker_id.to_string();
    let hb_cancel = heartbeat_cancel.clone();
    let hb_lease_lost = lease_lost.clone();
    let hb_job_id = job_id;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.tick().await; // first tick is immediate
        loop {
            tokio::select! {
                _ = hb_cancel.cancelled() => { return; }
                _ = interval.tick() => {
                    match hb_repo.heartbeat_ingestion_job(hb_job_id, &hb_worker).await {
                        Ok(true) => {} // lease still valid
                        Ok(false) => {
                            warn!(job_id = %hb_job_id, "Ingestion heartbeat rejected — lease reclaimed by another worker");
                            hb_lease_lost.cancel();
                            return;
                        }
                        Err(e) => {
                            warn!(job_id = %hb_job_id, error = %e, "Ingestion heartbeat failed — lease may be stale");
                        }
                    }
                }
            }
        }
    });

    // Open an ingestion run for provenance.
    let run_id = Uuid::new_v4();
    let run = spectraplex_core::v2::IngestionRun {
        id: run_id,
        target_id: job.target_id,
        network: job.network.clone(),
        source: network_to_source(&job.network),
        mode: job.mode,
        status: spectraplex_core::v2::IngestionJobStatus::Running,
        started_at: chrono::Utc::now(),
        finished_at: None,
        records_written: 0,
        error_message: None,
        cursor_state: None,
    };
    let run_created = repo.create_ingestion_run(&run).await.is_ok();
    if !run_created {
        warn!(job_id = %job_id, "Failed to create ingestion run; raw_transactions will not carry ingestion_run_id");
    }

    // Execute the actual ingestion.
    let result = run_ingestion(repo, config, provider_registry, job, run_id, run_created).await;

    // Stop heartbeat.
    heartbeat_cancel.cancel();

    // If lease was lost during execution, skip terminal state updates
    // to avoid conflicting with the worker that reclaimed the job.
    if lease_lost.is_cancelled() {
        warn!(job_id = %job_id, "Lease lost during ingestion — skipping terminal state update");
        // Best-effort: mark the ingestion run as failed so it doesn't stay 'running' forever.
        if run_created {
            let _ = repo
                .update_ingestion_run_status(
                    run_id,
                    "failed",
                    Some(chrono::Utc::now()),
                    0,
                    Some("lease lost"),
                )
                .await;
        }
        return;
    }

    match result {
        Ok(count) => {
            info!(job_id = %job_id, count, "Ingestion job completed");
            // Update ingestion run.
            let _ = repo
                .update_ingestion_run_status(
                    run_id,
                    "completed",
                    Some(chrono::Utc::now()),
                    usize_to_i64_or_max(count),
                    None,
                )
                .await;
            // Mark job complete. If this fails (e.g. lease reclaimed), do NOT
            // proceed to auto-enqueue materialization — the new owner will handle it.
            if let Err(e) = repo.complete_ingestion_job(job_id, worker_id).await {
                error!(job_id = %job_id, error = %e, "Failed to mark job completed — skipping auto-materialization");
            } else if run_created {
                // Auto-trigger materialization on successful ingestion.
                // Only enqueue a Bronze-driven run if the ingestion_run was actually
                // created — otherwise raw_transactions won't carry this run_id and
                // the materialize worker would find zero rows.
                if let Some(tid) = job.target_id {
                    match repo.get_index_target(tid).await {
                        Ok(Some(target)) => {
                            if target.kind == TargetKind::Wallet {
                                let mat_scope = serde_json::json!({
                                    "wallet": target.address.unwrap_or_default(),
                                    "network": &job.network,
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
                                    warn!(job_id = %job_id, error = %e, "Failed to enqueue post-ingest materialization (non-fatal)");
                                } else {
                                    info!(job_id = %job_id, "Enqueued post-ingest materialization run");
                                }
                            } else {
                                info!(job_id = %job_id, target_kind = ?target.kind, "Skipping wallet-scoped post-ingest materialization for non-wallet target");
                            }
                        }
                        Ok(None) => {
                            warn!(job_id = %job_id, target_id = %tid, "Index target not found for post-ingest materialization");
                        }
                        Err(e) => {
                            warn!(job_id = %job_id, error = %e, "Failed to fetch index target for post-ingest materialization");
                        }
                    }
                }
            }

            // Fire callback if present.
            if let Some(ref url) = job.callback_url {
                let payload = serde_json::json!({
                    "job_id": job_id,
                    "state": "completed",
                    "network": &job.network,
                    "message": format!("Ingested {} transactions", count),
                });
                fire_callback_best_effort(url, &payload, config.callback_hmac_secret.as_deref())
                    .await;
            }
        }
        Err(e) => {
            let err_msg = e.to_string();
            error!(job_id = %job_id, error = %err_msg, "Ingestion job failed");
            // Update ingestion run.
            let _ = repo
                .update_ingestion_run_status(
                    run_id,
                    "failed",
                    Some(chrono::Utc::now()),
                    0,
                    Some(&err_msg),
                )
                .await;
            // Mark job failed.
            if let Err(e2) = repo.fail_ingestion_job(job_id, worker_id, &err_msg).await {
                error!(job_id = %job_id, error = %e2, "Failed to mark job failed");
            }
            // Fire callback if present.
            if let Some(ref url) = job.callback_url {
                let payload = serde_json::json!({
                    "job_id": job_id,
                    "state": "failed",
                    "network": &job.network,
                    "message": err_msg,
                });
                fire_callback_best_effort(url, &payload, config.callback_hmac_secret.as_deref())
                    .await;
            }
        }
    }
}

/// Returns true when a target-centric ingestion job should use the V2
/// Connector abstraction instead of the legacy wallet-shaped ChainIngestor flow.
fn target_uses_connector_backfill(target: &IndexTarget) -> bool {
    matches!(
        (target.chain_family, target.kind),
        (
            spectraplex_core::v2::ChainFamily::Hyperliquid,
            TargetKind::Market
        )
    )
}

async fn run_connector_backfill(
    repo: &Repository,
    provider_registry: &ProviderRegistry,
    job: &IngestionJob,
    target: &IndexTarget,
    limit: usize,
    run_id: Uuid,
    run_created: bool,
) -> anyhow::Result<usize> {
    let net_ctx =
        NetworkContext::from_registry(provider_registry, &NetworkId::new(job.network.clone()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "network '{}' is not configured in the provider registry; cannot execute ingestion job {}",
                    job.network,
                    job.id
                )
            })?;

    let source = match target.chain_family {
        spectraplex_core::v2::ChainFamily::Solana => "rpc",
        spectraplex_core::v2::ChainFamily::Evm => "rpc",
        spectraplex_core::v2::ChainFamily::Hyperliquid => "rest",
    };
    let checkpoint_cursor = repo
        .get_checkpoint_v2(target.id, &job.network, source)
        .await?
        .map(|cp| cp.cursor);

    let batch: IngestionBatch = match (target.chain_family, target.kind) {
        (spectraplex_core::v2::ChainFamily::Hyperliquid, TargetKind::Market) => {
            let adapter = HyperliquidAdapter::from_network_context(&net_ctx);
            adapter
                .backfill(target, checkpoint_cursor.as_ref(), limit)
                .await?
        }
        _ => anyhow::bail!(
            "connector backfill is not supported for target kind {:?} on {:?}",
            target.kind,
            target.chain_family
        ),
    };

    let count = batch.records.len();
    let mut v2_records = batch.records;
    if run_created {
        for raw in &mut v2_records {
            raw.ingestion_run_id = Some(run_id);
        }
    }

    let mut seen = HashSet::new();
    let v2_deduped: Vec<RawTransaction> = v2_records
        .into_iter()
        .filter(|r| seen.insert((r.network.clone(), r.tx_hash.clone())))
        .collect();

    let canonical_ids = repo
        .upsert_raw_transactions_returning_ids(&v2_deduped)
        .await?;
    let matches = build_target_matches(target.id, &canonical_ids);
    repo.save_target_matches(&matches).await?;

    let derived_checkpoint = v2_deduped
        .iter()
        .filter_map(|raw| {
            raw.raw_metadata
                .get("data")
                .and_then(|data| data.get("time"))
                .and_then(|value| value.as_i64())
                .or_else(|| raw.timestamp.checked_mul(1000))
        })
        .max()
        .map(|last_time_ms| spectraplex_core::v2::Checkpoint {
            id: Uuid::new_v4(),
            target_id: target.id,
            network: job.network.clone(),
            source: source.to_string(),
            cursor: serde_json::json!({ "last_time_ms": last_time_ms }),
            updated_at: chrono::Utc::now(),
        });

    if let Some(cp) = batch.checkpoint.or(derived_checkpoint) {
        repo.upsert_checkpoint_v2(&cp).await?;
    }

    Ok(count)
}

/// Execute one ingestion job end-to-end and return how many records were written.
///
/// This mirrors the logic previously inline in `trigger_ingest`, but works
/// from the durable `IngestionJob` fields instead of the HTTP request body.
async fn run_ingestion(
    repo: &Repository,
    config: &AppConfig,
    provider_registry: &ProviderRegistry,
    job: &IngestionJob,
    run_id: Uuid,
    run_created: bool,
) -> anyhow::Result<usize> {
    let target_id = job
        .target_id
        .ok_or_else(|| anyhow::anyhow!("ingestion job {} has no target_id", job.id))?;
    let target = repo
        .get_index_target(target_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("target {} not found", target_id))?;

    if target.kind != TargetKind::Wallet && !target_uses_connector_backfill(&target) {
        anyhow::bail!(
            "ingestion job {} targets unsupported kind {:?} on {:?}",
            job.id,
            target.kind,
            target.chain_family
        );
    }

    if target_uses_connector_backfill(&target) {
        return run_connector_backfill(
            repo,
            provider_registry,
            job,
            &target,
            config.ingest_limit,
            run_id,
            run_created,
        )
        .await;
    }

    // Resolve chain family from network.
    let chain = network_to_chain(&job.network)?;
    let chain_str = match chain {
        Chain::Solana => "solana",
        Chain::Ethereum => "ethereum",
        Chain::Hyperliquid => "hyperliquid",
    };

    let wallet = target
        .address
        .clone()
        .ok_or_else(|| anyhow::anyhow!("target {} has no address", target_id))?;

    // Resolve V2 checkpoint for resume. For durable jobs we require a V2
    // network-scoped checkpoint. Falling back to the V1 chain-scoped
    // checkpoint is unsafe for EVM networks because "ethereum" checkpoints
    // are shared across Ethereum/Base/Arbitrum and would resume from the
    // wrong chain's cursor.
    let checkpoint = {
        let source = chain_to_default_source(&chain);
        match repo
            .get_checkpoint_v2(target_id, &job.network, source)
            .await
        {
            Ok(Some(v2_cp)) => {
                info!(
                    network = %job.network,
                    wallet = %wallet,
                    "Resuming from V2 checkpoint (network-scoped)"
                );
                Some(spectraplex_adapters::dual_write::v2_checkpoint_to_v1(
                    &v2_cp, &chain, &wallet,
                ))
            }
            Ok(None) => {
                info!(
                    network = %job.network,
                    wallet = %wallet,
                    "No V2 checkpoint found; starting from scratch"
                );
                None
            }
            Err(e) => {
                anyhow::bail!(
                    "V2 checkpoint lookup failed for network={}, wallet={}: {e}",
                    job.network,
                    wallet
                );
            }
        }
    };

    // Resolve network context from registry. Fail-closed: if the requested
    // network is not configured in the provider registry, reject the job
    // rather than silently falling back to a different chain's RPC endpoint.
    let net_ctx =
        NetworkContext::from_registry(provider_registry, &NetworkId::new(job.network.clone()))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "network '{}' is not configured in the provider registry; \
             cannot execute ingestion job {}",
                    job.network,
                    job.id
                )
            })?;

    let limit = config.ingest_limit;
    let user_id = requested_by_user_id(job.requested_by.as_deref(), target.owner_id);

    let events: Vec<Transaction> = match chain {
        Chain::Hyperliquid => {
            let adapter = HyperliquidAdapter::from_network_context(&net_ctx);
            adapter
                .fetch_history(&wallet, limit, user_id, checkpoint.as_ref())
                .await?
        }
        Chain::Ethereum => {
            let adapter = EvmAdapter::from_network_context(&net_ctx)?;
            adapter
                .fetch_history(&wallet, limit, user_id, checkpoint.as_ref())
                .await?
        }
        Chain::Solana => {
            let adapter = SolanaAdapter::from_network_context(&net_ctx)?;
            adapter
                .fetch_history(&wallet, limit, user_id, checkpoint.as_ref())
                .await?
        }
    };

    let count = events.len();

    // --- V2 AUTHORITATIVE WRITES (fail-closed) ---

    // Convert V1 txs to V2 RawTransactions
    let v2_records: Vec<RawTransaction> = events
        .iter()
        .map(|tx| {
            let mut raw = v1_tx_to_v2_raw(tx, Some(&job.network));
            if run_created {
                raw.ingestion_run_id = Some(run_id);
            }
            raw
        })
        .collect();

    let mut seen = HashSet::new();
    let v2_deduped: Vec<RawTransaction> = v2_records
        .into_iter()
        .filter(|r| seen.insert((r.network.clone(), r.tx_hash.clone())))
        .collect();

    let canonical_ids = repo
        .upsert_raw_transactions_returning_ids(&v2_deduped)
        .await?;

    let matches = build_target_matches(target_id, &canonical_ids);
    repo.save_target_matches(&matches).await?;

    if let Some(ref v1_cp) = build_checkpoint(chain_str, &wallet, &events) {
        let v2_cp = v1_checkpoint_to_v2_with_network(v1_cp, target_id, Some(&job.network));
        repo.upsert_checkpoint_v2(&v2_cp).await?;
    }

    if config.enable_v1_compat_writes {
        let v1_txs_ok = repo.save_transactions(&events).await.is_ok();
        if !v1_txs_ok {
            warn!(job_id = %job.id, "V1 compat: save_transactions failed, skipping V1 checkpoint (non-fatal)");
        }
        if v1_txs_ok {
            if let Some(ref v1_cp) = build_checkpoint(chain_str, &wallet, &events) {
                if let Err(e) = repo.save_checkpoint(v1_cp).await {
                    warn!(job_id = %job.id, error = %e, "V1 compat: save_checkpoint failed (non-fatal)");
                }
            }
        }
    }

    Ok(count)
}

/// Derive the V1 `Chain` enum from a V2 network ID.
fn network_to_chain(network: &str) -> anyhow::Result<Chain> {
    if network.starts_with("solana") {
        Ok(Chain::Solana)
    } else if network.starts_with("hypercore") || network.starts_with("hyperliquid") {
        Ok(Chain::Hyperliquid)
    } else {
        // Everything else is EVM (ethereum, base, arbitrum, polygon, etc.)
        Ok(Chain::Ethereum)
    }
}

/// Derive the default ingestion source from a network ID.
fn network_to_source(network: &str) -> String {
    if network.starts_with("solana") {
        "rpc".to_string()
    } else if network.starts_with("hypercore") || network.starts_with("hyperliquid") {
        "rest".to_string()
    } else {
        "rpc".to_string()
    }
}

fn usize_to_i64_or_max(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn requested_by_user_id(requested_by: Option<&str>, target_owner_id: Option<Uuid>) -> Uuid {
    let requested_by = requested_by
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if let Some(value) = requested_by {
        if let Ok(user_id) = Uuid::parse_str(value) {
            return user_id;
        }
    }

    if let Some(owner_id) = target_owner_id {
        return owner_id;
    }

    if let Some(value) = requested_by {
        let name = format!("spectraplex:ingestion:requested_by:{value}");
        return Uuid::new_v5(&Uuid::NAMESPACE_URL, name.as_bytes());
    }

    Uuid::new_v4()
}

/// Best-effort callback delivery using the SSRF-safe client. Pins DNS at
/// send time and disables redirects to prevent DNS rebinding and
/// redirect-based SSRF, matching the protections in `fire_callback`.
async fn fire_callback_best_effort(url: &str, payload: &serde_json::Value, secret: Option<&str>) {
    let client = match crate::build_ssrf_safe_client(url, Duration::from_secs(10)).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, url, "Rejected callback URL at delivery time (SSRF protection)");
            return;
        }
    };

    let body = match serde_json::to_vec(payload) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, url, "Failed to serialize callback payload");
            return;
        }
    };

    let mut req = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json");
    if let Some(secret) = secret {
        let signature = spectraplex_core::callback::sign_callback_payload(secret, &body);
        req = req.header("X-Spectraplex-Signature", format!("sha256={signature}"));
    }

    match req.body(body).send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                warn!(status = %resp.status(), url, "Callback returned non-success status");
            }
        }
        Err(e) => {
            warn!(error = %e, url, "Failed to deliver callback");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spectraplex_core::v2::{ChainFamily, IndexTarget, TargetKind, TargetMode};

    #[test]
    fn usize_to_i64_or_max_saturates() {
        assert_eq!(usize_to_i64_or_max(42), 42);
        assert_eq!(usize_to_i64_or_max(i64::MAX as usize), i64::MAX);
        assert_eq!(usize_to_i64_or_max(i64::MAX as usize + 1), i64::MAX);
        assert_eq!(usize_to_i64_or_max(usize::MAX), i64::MAX);
    }

    #[test]
    fn requested_by_user_id_accepts_uuid_requester() {
        let requested_by = Uuid::new_v4();
        let owner_id = Uuid::new_v4();

        assert_eq!(
            requested_by_user_id(Some(&requested_by.to_string()), Some(owner_id)),
            requested_by
        );
    }

    #[test]
    fn requested_by_user_id_uses_target_owner_for_legacy_requester() {
        let owner_id = Uuid::new_v4();

        assert_eq!(requested_by_user_id(Some("api"), Some(owner_id)), owner_id);
    }

    #[test]
    fn requested_by_user_id_uses_target_owner_when_requester_missing() {
        let owner_id = Uuid::new_v4();

        assert_eq!(requested_by_user_id(None, Some(owner_id)), owner_id);
    }

    #[test]
    fn requested_by_user_id_synthesizes_stable_legacy_requester_id() {
        let first = requested_by_user_id(Some("api"), None);
        let second = requested_by_user_id(Some("api"), None);
        let different = requested_by_user_id(Some("worker"), None);

        assert_eq!(first, second);
        assert_ne!(first, different);
    }

    #[test]
    fn requested_by_user_id_generates_when_requester_and_owner_missing() {
        assert_ne!(
            requested_by_user_id(None, None),
            requested_by_user_id(None, None)
        );
    }

    #[test]
    fn target_uses_connector_backfill_for_hyperliquid_market() {
        let now = chrono::Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Market,
            network: "hypercore-mainnet".to_string(),
            chain_family: ChainFamily::Hyperliquid,
            address: Some("ETH".to_string()),
            filter_spec: None,
            mode: TargetMode::Backfill,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };

        assert!(target_uses_connector_backfill(&target));
    }

    #[test]
    fn target_uses_connector_backfill_stays_legacy_for_wallets() {
        let now = chrono::Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: "hypercore-mainnet".to_string(),
            chain_family: ChainFamily::Hyperliquid,
            address: Some("0xabc".to_string()),
            filter_spec: None,
            mode: TargetMode::Backfill,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };

        assert!(!target_uses_connector_backfill(&target));
    }
}
