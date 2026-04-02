//! Background materialization worker loop.
//!
//! Polls for pending or stale-heartbeat `materialization_runs` rows,
//! claims them, executes the normalize work, and completes/fails them
//! durably with heartbeats.

use std::sync::Arc;
use std::time::Duration;

use crate::fire_callback;
use spectraplex_adapters::dual_write::v2_raw_to_v1_tx;
use spectraplex_adapters::{evm_parser, hyperliquid_parser, repo::Repository, solana_parser};
use spectraplex_core::models::Chain;
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
    job_semaphore: Arc<Semaphore>,
    cancel: CancellationToken,
) {
    let worker_id = format!("mat-worker-{}", Uuid::new_v4());
    info!(worker_id = %worker_id, "Starting materialize worker");

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(worker_id = %worker_id, "Materialize worker shutting down");
                    return;
                }
                _ = worker_tick(&repo, &worker_id, &job_semaphore) => {}
            }
        }
    });
}

async fn worker_tick(repo: &Repository, worker_id: &str, job_semaphore: &Arc<Semaphore>) {
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
        let (wallet, network, callback_url, ingestion_run_id) = match &run.scope {
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
            let result = execute_normalize(
                &task_repo,
                &wallet,
                network.as_deref(),
                ingestion_run_id,
                &lease_lost,
            )
            .await;

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
                Ok(count) => {
                    info!(run_id = %run_id, count, "Materialization run completed (worker)");
                    match task_repo
                        .complete_materialization_run(run_id, &task_worker_id, count as i64)
                        .await
                    {
                        Ok(()) => {
                            // Upsert dataset watermark after successful materialization.
                            if let Some(irun_id) = ingestion_run_id {
                                let scope_json = serde_json::json!({
                                    "network": network,
                                    "wallet": wallet,
                                });
                                if let Err(e) = task_repo
                                    .upsert_dataset_watermark(
                                        "normalize",
                                        Some(&scope_json),
                                        Some(irun_id),
                                        None,
                                    )
                                    .await
                                {
                                    warn!(run_id = %run_id, error = %e, "Failed to upsert dataset watermark (non-fatal)");
                                }
                            }

                            // Terminal write succeeded — safe to fire callback.
                            (
                                Some("completed"),
                                Some(format!("Normalized {} ledger entries", count)),
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
                fire_callback(url, &payload).await;
            }
        });
    }
}

/// Core normalize logic used by the background worker.
///
/// Accepts a `lease_lost` token that is checked before each side-effecting
/// operation (`save_ledger_entries`, `materialize_silver_datasets`). If the
/// lease has been reclaimed by another worker while we were parsing, we bail
/// early instead of producing duplicate writes.
///
/// NOTE: There is a narrow window between the pre-save lease check and the
/// actual `save_ledger_entries` call where the lease could be lost. Making
/// `save_ledger_entries` cancellation-aware would be needed to fully close
/// this gap; we accept this as a known limitation and check again after save.
pub(crate) async fn execute_normalize(
    repo: &Repository,
    wallet: &str,
    network: Option<&str>,
    ingestion_run_id: Option<Uuid>,
    lease_lost: &CancellationToken,
) -> anyhow::Result<usize> {
    let txs = if let Some(run_id) = ingestion_run_id {
        // Bronze-driven: fetch raw_transactions for this specific ingestion run
        // and convert V2 RawTransaction -> V1 Transaction for existing parsers.
        //
        // Safety: validate that the ingestion_run's target address matches the
        // caller-supplied wallet so we never project another wallet's Bronze
        // rows into this wallet's ledger/silver data.
        if let Some(irun) = repo.get_ingestion_run(run_id).await? {
            if let Some(tid) = irun.target_id {
                if let Ok(Some(target)) = repo.get_index_target(tid).await {
                    let target_addr = target.address.unwrap_or_default();
                    if !target_addr.is_empty()
                        && target_addr.to_lowercase() != wallet.to_lowercase()
                    {
                        anyhow::bail!(
                            "ingestion_run {} belongs to target wallet {}, not {}",
                            run_id,
                            target_addr,
                            wallet
                        );
                    }
                }
            }
        }

        let raw_txs = repo.get_raw_transactions_by_run(run_id).await?;
        if raw_txs.is_empty() {
            // Empty Bronze range is a valid outcome (e.g. zero new transactions
            // for this ingestion). Return empty vec — do NOT fall back to the
            // legacy wallet scan, which would rematerialize stale historical
            // data and produce duplicate ledger/silver output.
            info!(
                run_id = %run_id,
                "Bronze-driven materialization: no raw transactions found for run, returning 0 records"
            );
            Vec::new()
        } else {
            info!(
                run_id = %run_id,
                raw_count = raw_txs.len(),
                "Bronze-driven materialization: fetched raw transactions"
            );
            raw_txs
                .iter()
                .filter_map(|r| {
                    let chain = network_to_chain(&r.network).ok()?;
                    Some(v2_raw_to_v1_tx(r, wallet, chain, None))
                })
                .collect()
        }
    } else {
        // Legacy fallback: wallet-scoped V1 scan
        repo.get_transactions_by_wallet(wallet).await?
    };

    let mut all_entries = Vec::new();
    for tx in &txs {
        let result = match tx.chain {
            Chain::Solana => solana_parser::parse_solana_transaction(tx),
            Chain::Hyperliquid => hyperliquid_parser::parse_hyperliquid_transaction(tx),
            Chain::Ethereum => evm_parser::parse_evm_transaction(tx),
        };
        match result {
            Ok(entries) => all_entries.extend(entries),
            Err(e) => {
                error!(tx_hash = %tx.tx_hash, error = %e, "Skipping unparseable transaction");
            }
        }
    }

    // Check lease before persisting side effects — if we lost the lease
    // during parsing, another worker may already be running. Persisting
    // now would duplicate ledger entries (fresh UUIDs each run) and
    // re-trigger Silver materialization.
    if lease_lost.is_cancelled() {
        anyhow::bail!("lease lost before persisting side effects");
    }

    let count = all_entries.len();
    repo.save_ledger_entries(&all_entries).await?;

    if lease_lost.is_cancelled() {
        anyhow::bail!("lease lost before Silver materialization");
    }

    repo.materialize_silver_datasets(&txs, network).await;
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
