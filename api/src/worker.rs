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
use spectraplex_core::models::{Chain, ChainIngestor, Transaction};
use spectraplex_core::provider::{NetworkContext, NetworkId, ProviderRegistry};
use spectraplex_core::v2::IngestionJob;
use spectraplex_core::v2::RawTransaction;
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
    let heartbeat_cancel = CancellationToken::new();
    let hb_repo = repo.clone();
    let hb_worker = worker_id.to_string();
    let hb_cancel = heartbeat_cancel.clone();
    let hb_job_id = job_id;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.tick().await; // first tick is immediate
        loop {
            tokio::select! {
                _ = hb_cancel.cancelled() => { return; }
                _ = interval.tick() => {
                    if let Err(e) = hb_repo.heartbeat_ingestion_job(hb_job_id, &hb_worker).await {
                        warn!(job_id = %hb_job_id, error = %e, "Heartbeat failed");
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
        mode: job.mode.to_string(),
        status: "running".to_string(),
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

    match result {
        Ok(count) => {
            info!(job_id = %job_id, count, "Ingestion job completed");
            // Update ingestion run.
            let _ = repo
                .update_ingestion_run_status(
                    run_id,
                    "completed",
                    Some(chrono::Utc::now()),
                    count as i64,
                    None,
                )
                .await;
            // Mark job complete.
            if let Err(e) = repo.complete_ingestion_job(job_id, worker_id).await {
                error!(job_id = %job_id, error = %e, "Failed to mark job completed");
            }
            // Auto-trigger materialization on successful ingestion.
            // Only enqueue a Bronze-driven run if the ingestion_run was actually
            // created — otherwise raw_transactions won't carry this run_id and
            // the materialize worker would find zero rows.
            if run_created {
                if let Some(tid) = job.target_id {
                    match repo.get_index_target(tid).await {
                        Ok(Some(target)) => {
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
                        }
                        Ok(None) => {
                            warn!(job_id = %job_id, target_id = %tid, "Index target not found for post-ingest materialization");
                        }
                        Err(e) => {
                            warn!(job_id = %job_id, error = %e, "Failed to fetch index target for post-ingest materialization");
                        }
                    }
                }
            } // if run_created

            // Fire callback if present.
            if let Some(ref url) = job.callback_url {
                let payload = serde_json::json!({
                    "job_id": job_id,
                    "state": "completed",
                    "network": &job.network,
                    "message": format!("Ingested {} transactions", count),
                });
                fire_callback_best_effort(url, &payload).await;
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
                fire_callback_best_effort(url, &payload).await;
            }
        }
    }
}

/// Execute the actual chain ingestion for a job.
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
    // Resolve chain family from network.
    let chain = network_to_chain(&job.network)?;
    let chain_str = match chain {
        Chain::Solana => "solana",
        Chain::Ethereum => "ethereum",
        Chain::Hyperliquid => "hyperliquid",
    };

    // Resolve wallet address from the target.
    let wallet = if let Some(target_id) = job.target_id {
        let target = repo
            .get_index_target(target_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("target {} not found", target_id))?;
        target
            .address
            .ok_or_else(|| anyhow::anyhow!("target {} has no address", target_id))?
    } else {
        anyhow::bail!("ingestion job {} has no target_id", job.id);
    };

    let target_id = job.target_id;

    // Resolve V2 checkpoint for resume. For durable jobs we require a V2
    // network-scoped checkpoint. Falling back to the V1 chain-scoped
    // checkpoint is unsafe for EVM networks because "ethereum" checkpoints
    // are shared across Ethereum/Base/Arbitrum and would resume from the
    // wrong chain's cursor.
    let checkpoint = if let Some(tid) = target_id {
        let source = chain_to_default_source(&chain);
        match repo.get_checkpoint_v2(tid, &job.network, source).await {
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
                // Surface the error rather than silently falling back.
                anyhow::bail!(
                    "V2 checkpoint lookup failed for network={}, wallet={}: {e}",
                    job.network,
                    wallet
                );
            }
        }
    } else {
        None
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
    let user_id = job
        .requested_by
        .as_ref()
        .and_then(|s| s.parse::<Uuid>().ok())
        .unwrap_or_else(Uuid::new_v4);

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
            // Only stamp ingestion_run_id if the run row was created;
            // otherwise the FK would reject the insert.
            if run_created {
                raw.ingestion_run_id = Some(run_id);
            }
            raw
        })
        .collect();

    // Deduplicate by (network, tx_hash) — same tx may appear from different calls
    let mut seen = HashSet::new();
    let v2_deduped: Vec<RawTransaction> = v2_records
        .into_iter()
        .filter(|r| seen.insert((r.network.clone(), r.tx_hash.clone())))
        .collect();

    // Write raw_transactions (authoritative)
    let canonical_ids = repo
        .upsert_raw_transactions_returning_ids(&v2_deduped)
        .await?;

    // Write target_matches (authoritative)
    if let Some(tid) = target_id {
        let matches = build_target_matches(tid, &canonical_ids);
        repo.save_target_matches(&matches).await?;
    }

    // Write V2 checkpoint (authoritative)
    if let Some(ref v1_cp) = build_checkpoint(chain_str, &wallet, &events) {
        if let Some(tid) = target_id {
            let v2_cp = v1_checkpoint_to_v2_with_network(v1_cp, tid, Some(&job.network));
            repo.upsert_checkpoint_v2(&v2_cp).await?;
        }
    }

    // --- V1 COMPATIBILITY PROJECTION (best-effort, logged but non-fatal) ---
    //
    // Only advance the V1 checkpoint if save_transactions succeeded. If the
    // V1 transaction write fails (even partially), advancing the checkpoint
    // would cause legacy readers to skip those transactions permanently on
    // the next incremental run.

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

/// Best-effort callback delivery using the SSRF-safe client. Pins DNS at
/// send time and disables redirects to prevent DNS rebinding and
/// redirect-based SSRF, matching the protections in `fire_callback`.
async fn fire_callback_best_effort(url: &str, payload: &serde_json::Value) {
    let client = match crate::build_ssrf_safe_client(url, Duration::from_secs(10)).await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, url, "Rejected callback URL at delivery time (SSRF protection)");
            return;
        }
    };
    match client.post(url).json(payload).send().await {
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
