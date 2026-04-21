//! Background materialization worker loop.
//!
//! Polls for pending or stale-heartbeat `materialization_runs` rows,
//! claims them, executes the normalize work, and completes/fails them
//! durably with heartbeats.

use std::sync::Arc;
use std::time::Duration;

use crate::fire_callback;
use spectraplex_adapters::dual_write::BronzeSilverResult;
use spectraplex_adapters::repo::Repository;
use spectraplex_core::config::AppConfig;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

/// How often the worker polls for new runs when idle.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often the worker sends heartbeats while executing a run.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the materialize worker loop. Returns immediately; the worker runs
/// until `cancel` is triggered.
pub fn spawn_materialize_worker(
    repo: Repository,
    config: AppConfig,
    job_semaphore: Arc<Semaphore>,
    cancel: CancellationToken,
) {
    let worker_id = format!("mat-worker-{}", Uuid::new_v4());
    info!(worker_id = %worker_id, "Starting materialize worker");
    let callback_hmac_secret = config.callback_hmac_secret.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(worker_id = %worker_id, "Materialize worker shutting down");
                    return;
                }
                _ = worker_tick(&repo, &worker_id, &job_semaphore, callback_hmac_secret.clone()) => {}
            }
        }
    });
}

async fn worker_tick(repo: &Repository, worker_id: &str, job_semaphore: &Arc<Semaphore>, callback_hmac_secret: Option<String>) {
    let runs = match repo.list_claimable_materialization_runs(5).await {
        Ok(runs) => runs,
        Err(e) => {
            error!(error = %e, "Failed to list claimable materialization runs");
            tokio::time::sleep(POLL_INTERVAL).await;
            return;
        }
    };

    if runs.is_empty() {
        tokio::time::sleep(POLL_INTERVAL).await;
        return;
    }

    for run in runs {
        // Acquire a semaphore permit before claiming; if at capacity, stop
        // claiming more runs this tick and let in-flight tasks finish.
        let permit = match job_semaphore.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                // At capacity — sleep before returning so we don't hot-loop
                // on list_claimable while all permits are held.
                tokio::time::sleep(POLL_INTERVAL).await;
                return;
            }
        };

        let claimed = match repo.claim_materialization_run(run.id, worker_id).await {
            Ok(c) => c,
            Err(e) => {
                warn!(run_id = %run.id, error = %e, "Failed to claim materialization run");
                continue;
            }
        };
        if !claimed {
            // Another worker got it first — that's fine.
            continue;
        }

        info!(run_id = %run.id, worker_id = %worker_id, "Claimed materialization run");

        // Extract wallet, network, callback_url, and optional ingestion_run_id from scope JSON.
        let (wallet, _network, callback_url, ingestion_run_id) = match &run.scope {
            Some(scope) => {
                let w = scope
                    .get("wallet")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let n = scope
                    .get("network")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let cb = scope
                    .get("callback_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let run_id = scope
                    .get("ingestion_run_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok());
                (w, n, cb, run_id)
            }
            None => {
                let _ = repo
                    .fail_materialization_run(run.id, worker_id, "missing scope")
                    .await;
                continue;
            }
        };

        if wallet.is_empty() {
            let _ = repo
                .fail_materialization_run(run.id, worker_id, "empty wallet in scope")
                .await;
            continue;
        }

        // Spawn the execution as a concurrent task so the worker loop can
        // continue claiming more runs while this one executes.
        let task_repo = repo.clone();
        let task_worker_id = worker_id.to_string();
        let run_id = run.id;
        let task_hmac_secret = callback_hmac_secret.clone();

        tokio::spawn(async move {
            // Hold the semaphore permit for the duration of this task.
            let _permit = permit;

            // Heartbeat + lease-lost token.
            let hb_cancel = CancellationToken::new();
            let hb_token = hb_cancel.clone();
            let lease_lost = CancellationToken::new();
            let lease_lost_inner = lease_lost.clone();
            let hb_repo = task_repo.clone();
            let hb_wid = task_worker_id.clone();
            let hb_task = tokio::spawn(async move {
                let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
                loop {
                    tokio::select! {
                        _ = hb_token.cancelled() => break,
                        _ = interval.tick() => {
                            match hb_repo.heartbeat_materialization_run(run_id, &hb_wid).await {
                                Ok(true) => {}
                                Ok(false) => {
                                    warn!(run_id = %run_id, "Lease lost for materialization run (worker)");
                                    lease_lost_inner.cancel();
                                    break;
                                }
                                Err(e) => {
                                    warn!("heartbeat failed: {e}");
                                }
                            }
                        }
                    }
                }
            });

            // Execute normalize work — pass lease_lost so side effects are
            // skipped if the lease is reclaimed mid-execution.
            let result =
                execute_normalize(&task_repo, &wallet, ingestion_run_id, &lease_lost).await;

            // Check lease_lost BEFORE writing terminal state. If the lease was
            // lost (either detected by heartbeat or by execute_normalize's own
            // checks), we no longer own this run and must not update it.
            if lease_lost.is_cancelled() {
                warn!(run_id = %run_id, "Lease lost, skipping terminal update (worker)");
                hb_cancel.cancel();
                hb_task.abort();
                return;
            }

            // Write terminal state to DB while heartbeat is still running, so
            // the row isn't reclaimed by another worker during a transient DB
            // delay (FIX 1: heartbeat stops AFTER terminal write).
            let (callback_state, callback_message) = match result {
                Ok(silver_result) => {
                    let count = silver_result.total_written;
                    info!(
                        run_id = %run_id,
                        written = count,
                        failed = silver_result.total_failed,
                        "Materialization run completed (worker)"
                    );
                    match task_repo
                        .complete_materialization_run(run_id, &task_worker_id, count as i64)
                        .await
                    {
                        Ok(()) => {
                            // Upsert dataset watermark after successful materialization.
                            // When Bronze-driven, resolve the authoritative wallet/network
                            // from the ingestion run so watermarks don't fork across EVM
                            // case variants or caller-supplied mismatches.
                            if let Some(irun_id) = ingestion_run_id {
                                // Resolve authoritative wallet/network from the
                                // ingestion run. Skip the watermark entirely if
                                // the lookup fails — we must not write a watermark
                                // under non-authoritative scope values.
                                let wm_resolved = match task_repo.get_ingestion_run(irun_id).await {
                                    Ok(Some(irun)) => {
                                        let w = if let Some(tid) = irun.target_id {
                                            task_repo
                                                .get_index_target(tid)
                                                .await
                                                .ok()
                                                .flatten()
                                                .and_then(|t| t.address)
                                        } else {
                                            None
                                        };
                                        w.map(|wallet_addr| (wallet_addr, irun.network.clone()))
                                    }
                                    _ => None,
                                };
                                if !silver_result.all_succeeded() {
                                    warn!(
                                        run_id = %run_id,
                                        total_failed = silver_result.total_failed,
                                        "Watermark not advanced due to partial Silver failures"
                                    );
                                } else if let Some((wm_wallet, wm_network)) = wm_resolved {
                                    let scope_json = serde_json::json!({
                                        "network": wm_network,
                                        "wallet": wm_wallet,
                                    });
                                    if let Err(e) = task_repo
                                        .upsert_dataset_watermark(
                                            "normalize",
                                            Some(&scope_json),
                                            Some(irun_id),
                                            silver_result.last_raw_transaction_id,
                                        )
                                        .await
                                    {
                                        warn!(run_id = %run_id, error = %e, "Failed to upsert dataset watermark (non-fatal)");
                                    }
                                } else {
                                    warn!(run_id = %run_id, "Could not resolve authoritative scope for watermark — skipping");
                                }
                            }

                            // Terminal write succeeded — safe to fire callback.
                            (
                                Some("completed"),
                                Some(format!("Materialized {} Silver records", count)),
                            )
                        }
                        Err(e) => {
                            error!(run_id = %run_id, error = %e, "Failed to mark run completed");
                            // DB update failed — skip callback (FIX 4).
                            (None, None)
                        }
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    error!(run_id = %run_id, error = %msg, "Materialization run failed (worker)");

                    // If the error indicates a lease-lost condition, do NOT call
                    // fail_materialization_run — we no longer own the lease and
                    // the row may already be claimed by another worker (FIX 2).
                    if msg.contains("lease lost") {
                        warn!(run_id = %run_id, "Skipping fail_materialization_run due to lease-lost error");
                        (None, None)
                    } else {
                        match task_repo
                            .fail_materialization_run(run_id, &task_worker_id, &msg)
                            .await
                        {
                            Ok(()) => (Some("failed"), Some(msg)),
                            Err(db_err) => {
                                error!(run_id = %run_id, error = %db_err, "Failed to mark run failed");
                                // DB update failed — skip callback (FIX 4).
                                (None, None)
                            }
                        }
                    }
                }
            };

            // Now stop the heartbeat AFTER the terminal DB write (FIX 1).
            hb_cancel.cancel();
            hb_task.abort();

            // Fire durable callback only if the terminal DB update succeeded (FIX 4).
            if let (Some(ref url), Some(state), Some(message)) =
                (&callback_url, callback_state, callback_message)
            {
                let payload = serde_json::json!({
                    "job_id": run_id,
                    "state": state,
                    "wallet": wallet,
                    "message": message,
                });
                fire_callback(url, &payload, task_hmac_secret.as_deref()).await;
            }
        });
    }
}

/// Core normalize logic used by the background worker.
///
/// Accepts a `lease_lost` token that is checked before the Bronze-native
/// Silver materialization side effect. If the lease has been reclaimed by
/// another worker, we bail early instead of producing duplicate writes.
///
/// NOTE: There is a narrow window between the pre-save lease check and the
/// actual `materialize_silver_from_bronze` call where the lease could be lost.
/// Making `materialize_silver_from_bronze` cancellation-aware would be needed
/// to fully close this gap; we accept this as a known limitation.
pub(crate) async fn execute_normalize(
    repo: &Repository,
    wallet: &str,
    ingestion_run_id: Option<Uuid>,
    lease_lost: &CancellationToken,
) -> anyhow::Result<BronzeSilverResult> {
    // When Bronze-driven, we resolve the authoritative wallet from the target.
    let effective_wallet: String;

    if let Some(run_id) = ingestion_run_id {
        // Bronze-driven: fetch raw_transactions for this specific ingestion run
        // and materialize Silver datasets directly via Bronze-native path.
        //
        // Safety: validate that the ingestion_run exists and its target address
        // matches the caller-supplied wallet. Fail-closed: if any lookup fails
        // or the run/target is missing, we bail rather than silently proceeding.
        let irun = repo
            .get_ingestion_run(run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("ingestion_run {} not found", run_id))?;
        let tid = irun
            .target_id
            .ok_or_else(|| anyhow::anyhow!("ingestion_run {} has no target_id", run_id))?;
        let target = repo.get_index_target(tid).await?.ok_or_else(|| {
            anyhow::anyhow!(
                "index_target {} for ingestion_run {} not found",
                tid,
                run_id
            )
        })?;
        let target_addr = target.address.ok_or_else(|| {
            anyhow::anyhow!(
                "index_target {} for ingestion_run {} has no address",
                tid,
                run_id
            )
        })?;
        if target_addr.is_empty() {
            anyhow::bail!(
                "index_target {} for ingestion_run {} has empty address",
                tid,
                run_id
            );
        }
        if !addrs_match(&target_addr, wallet) {
            anyhow::bail!(
                "ingestion_run {} belongs to target wallet {}, not {}",
                run_id,
                target_addr,
                wallet
            );
        }
        // Use the authoritative wallet address from the target for deterministic
        // ID generation and watermark keying, preventing EVM case-variant forks.
        effective_wallet = target_addr;

        let raw_txs = repo.get_raw_transactions_by_run(run_id).await?;
        if raw_txs.is_empty() {
            info!(
                run_id = %run_id,
                "Bronze-driven materialization: no raw transactions found for run, returning 0 records"
            );
            return Ok(BronzeSilverResult::default());
        }

        info!(
            run_id = %run_id,
            raw_count = raw_txs.len(),
            "Bronze-driven materialization: fetched raw transactions"
        );

        // Check lease before persisting side effects.
        if lease_lost.is_cancelled() {
            anyhow::bail!("lease lost before persisting side effects");
        }

        // Bronze-native Silver materialization: extract Silver records directly
        // from RawTransaction without reconstructing V1 Transaction values.
        let silver_result = repo
            .materialize_silver_from_bronze(&raw_txs, Some(&effective_wallet))
            .await;

        if !silver_result.all_succeeded() {
            warn!(
                run_id = %run_id,
                written = silver_result.total_written,
                failed = silver_result.total_failed,
                "Bronze-native Silver materialization had partial failures"
            );
        }

        return Ok(silver_result);
    }

    anyhow::bail!("normalize requires an ingestion_run_id; enqueue a materialization run from an ingestion job or stream flush")
}

/// Chain-aware address comparison: case-insensitive for EVM (0x-prefixed),
/// exact match for everything else (Solana base58 is case-sensitive).
fn addrs_match(a: &str, b: &str) -> bool {
    let is_evm = a.starts_with("0x") || a.starts_with("0X");
    if is_evm {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}
