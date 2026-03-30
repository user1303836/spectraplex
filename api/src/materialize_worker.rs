//! Background materialization worker loop.
//!
//! Polls for pending or stale-heartbeat `materialization_runs` rows,
//! claims them, executes the normalize work, and completes/fails them
//! durably with heartbeats.

use std::time::Duration;

use crate::fire_callback;
use spectraplex_adapters::{evm_parser, hyperliquid_parser, repo::Repository, solana_parser};
use spectraplex_core::models::Chain;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

/// How often the worker polls for new runs when idle.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often the worker sends heartbeats while executing a run.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the materialize worker loop. Returns immediately; the worker runs
/// until `cancel` is triggered.
pub fn spawn_materialize_worker(repo: Repository, cancel: CancellationToken) {
    let worker_id = format!("mat-worker-{}", Uuid::new_v4());
    info!(worker_id = %worker_id, "Starting materialize worker");

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(worker_id = %worker_id, "Materialize worker shutting down");
                    return;
                }
                _ = worker_tick(&repo, &worker_id) => {}
            }
        }
    });
}

async fn worker_tick(repo: &Repository, worker_id: &str) {
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

        // Extract wallet, network, and callback_url from scope JSON.
        let (wallet, network, callback_url) = match &run.scope {
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
                (w, n, cb)
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

        // Heartbeat + lease-lost token.
        let hb_cancel = CancellationToken::new();
        let hb_token = hb_cancel.clone();
        let lease_lost = CancellationToken::new();
        let lease_lost_inner = lease_lost.clone();
        let hb_repo = repo.clone();
        let hb_run_id = run.id;
        let hb_wid = worker_id.to_string();
        let hb_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            loop {
                tokio::select! {
                    _ = hb_token.cancelled() => break,
                    _ = interval.tick() => {
                        match hb_repo.heartbeat_materialization_run(hb_run_id, &hb_wid).await {
                            Ok(true) => {}
                            Ok(false) => {
                                warn!(run_id = %hb_run_id, "Lease lost for materialization run (worker)");
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
        let result = execute_normalize(repo, &wallet, network.as_deref(), &lease_lost).await;

        // Stop heartbeat.
        hb_cancel.cancel();
        hb_task.abort();

        if lease_lost.is_cancelled() {
            warn!(run_id = %run.id, "Lease lost, skipping terminal update (worker)");
            continue;
        }

        let (final_state, final_message) = match result {
            Ok(count) => {
                info!(run_id = %run.id, count, "Materialization run completed (worker)");
                if let Err(e) = repo
                    .complete_materialization_run(run.id, worker_id, count as i64)
                    .await
                {
                    error!(run_id = %run.id, error = %e, "Failed to mark run completed");
                }
                ("completed", format!("Normalized {} ledger entries", count))
            }
            Err(e) => {
                let msg = e.to_string();
                error!(run_id = %run.id, error = %msg, "Materialization run failed (worker)");
                if let Err(db_err) = repo.fail_materialization_run(run.id, worker_id, &msg).await {
                    error!(run_id = %run.id, error = %db_err, "Failed to mark run failed");
                }
                ("failed", msg)
            }
        };

        // Fire durable callback if one was persisted in scope.
        if let Some(ref url) = callback_url {
            let payload = serde_json::json!({
                "job_id": run.id,
                "state": final_state,
                "wallet": wallet,
                "message": final_message,
            });
            fire_callback(url, &payload).await;
        }
    }
}

/// Core normalize logic used by the background worker.
///
/// Accepts a `lease_lost` token that is checked before each side-effecting
/// operation (`save_ledger_entries`, `materialize_silver_datasets`). If the
/// lease has been reclaimed by another worker while we were parsing, we bail
/// early instead of producing duplicate writes.
pub(crate) async fn execute_normalize(
    repo: &Repository,
    wallet: &str,
    network: Option<&str>,
    lease_lost: &CancellationToken,
) -> anyhow::Result<usize> {
    let txs = repo.get_transactions_by_wallet(wallet).await?;

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
