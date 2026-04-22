//! Durable export worker loop.
//!
//! Claims pending `export_jobs` rows from Postgres, streams the export
//! to disk via [`crate::export_stream::write_export_to_file`], optionally
//! delivers to a configured sink, and marks jobs complete/failed
//! durably. Heartbeats while running so that dead workers are detected
//! and their jobs reclaimed.
//!
//! Lease safety (PR #238 [P1] fix):
//!
//! The worker writes the export artifact to an *attempt-scoped* temp
//! file named `{job_id}.{worker_hash}.{ext}.tmp`, and only atomically
//! renames it to the canonical `{job_id}.{ext}` download path after a
//! lease-holder-confirming `update_export_job_status(..., worker_id)`
//! has returned `Ok(true)`. If the heartbeat loop reports the job's
//! lease was reclaimed, a `lease_lost` [`CancellationToken`] is
//! cancelled — that token is propagated into the page loop in
//! `write_export_to_file`, so the stale worker short-circuits the DB
//! snapshot, skips the rename, and removes its temp file. The new
//! owner starts from a clean state and writes to a distinct temp path,
//! so the two workers cannot truncate each other's artifacts or hand
//! out partial bytes via `/download`.

use std::sync::Arc;
use std::time::Duration;

use spectraplex_adapters::repo::Repository;
use spectraplex_core::config::AppConfig;
use spectraplex_core::materializer::{DeliveryMetadata, ExportFormat, SinkConfig};
use spectraplex_core::v2::ExportJob;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::build_sink;
use crate::export_stream::write_export_to_file;

/// How often the worker polls for new jobs when idle.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How often the worker sends heartbeats while executing a job.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Spawn the export worker loop. Returns immediately; the worker runs
/// until `cancel` is triggered.
pub fn spawn_export_worker(
    repo: Repository,
    config: AppConfig,
    job_semaphore: Arc<Semaphore>,
    cancel: CancellationToken,
) {
    let worker_id = format!("export-worker-{}", Uuid::new_v4());
    info!(worker_id = %worker_id, "Starting export worker");

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(worker_id = %worker_id, "Export worker shutting down");
                    return;
                }
                _ = worker_tick(&repo, &config, &job_semaphore, &worker_id) => {}
            }
        }
    });
}

async fn worker_tick(
    repo: &Repository,
    config: &AppConfig,
    job_semaphore: &Arc<Semaphore>,
    worker_id: &str,
) {
    let permit = match Arc::clone(job_semaphore).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            tokio::time::sleep(POLL_INTERVAL).await;
            return;
        }
    };

    match repo.claim_export_job(worker_id).await {
        Ok(Some(job)) => {
            info!(
                job_id = %job.id,
                dataset = %job.dataset,
                format = %job.format,
                worker_id = %worker_id,
                "Claimed export job"
            );
            execute_export_job(repo, config, worker_id, &job, permit).await;
        }
        Ok(None) => {
            drop(permit);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        Err(e) => {
            drop(permit);
            error!(error = %e, worker_id = %worker_id, "Failed to claim export job");
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}

/// Build an attempt-scoped temp path for this worker's export. Two
/// workers that race on the same `job_id` get different temp paths, so
/// a stale worker cannot truncate the new owner's artifact.
fn attempt_temp_path(exports_dir: &str, job_id: Uuid, worker_id: &str, ext: &str) -> String {
    // Short, filesystem-safe tag derived from the worker id. FNV-style
    // hash is fine — collision risk is negligible at the (active worker
    // count) scale this operates at.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    worker_id.hash(&mut h);
    let tag = format!("{:016x}", h.finish());
    format!("{exports_dir}/{job_id}.{tag}.{ext}.tmp")
}

/// Best-effort remove a file, swallowing NotFound and logging other
/// errors. Used by the failure paths in `execute_export_job` to clean
/// up attempt-scoped temp artifacts without masking the real error.
async fn best_effort_unlink(path: &str, job_id: Uuid) {
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            warn!(
                job_id = %job_id,
                error = %e,
                file = %path,
                "Failed to clean up temp export artifact",
            );
        }
    }
}

async fn execute_export_job(
    repo: &Repository,
    config: &AppConfig,
    worker_id: &str,
    job: &ExportJob,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let job_id = job.id;
    let export_dir = &config.export_dir;

    // `heartbeat_cancel` stops the heartbeat task when the export is
    // done. `lease_lost` is a *distinct* token that the heartbeat task
    // flips when `heartbeat_export_job` reports the job has been
    // reclaimed. Downstream code (the streaming writer and the final
    // rename) checks `lease_lost` to short-circuit its side effects
    // so a stale worker cannot publish a partial/bogus artifact over
    // the new owner's work.
    let heartbeat_cancel = CancellationToken::new();
    let lease_lost = CancellationToken::new();
    {
        let hb_repo = repo.clone();
        let hb_worker = worker_id.to_string();
        let hb_cancel = heartbeat_cancel.clone();
        let hb_lost = lease_lost.clone();
        let hb_job_id = job_id;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            interval.tick().await; // first tick is immediate
            loop {
                tokio::select! {
                    _ = hb_cancel.cancelled() => { return; }
                    _ = interval.tick() => {
                        match hb_repo.heartbeat_export_job(hb_job_id, &hb_worker).await {
                            Ok(true) => {}
                            Ok(false) => {
                                warn!(job_id = %hb_job_id, "Heartbeat returned false — lease lost");
                                // Propagate to the write loop so it stops
                                // before any more side effects.
                                hb_lost.cancel();
                                return;
                            }
                            Err(e) => {
                                warn!(job_id = %hb_job_id, error = %e, "Heartbeat failed");
                            }
                        }
                    }
                }
            }
        });
    }

    let (target_id, network, time_start, time_end) = parse_filters(&job.filters);

    let format = job.format;

    let ext = match format {
        ExportFormat::Jsonl => "jsonl",
        ExportFormat::Csv => "csv",
    };

    let exports_dir = format!("{}/exports", export_dir);
    if let Err(e) = tokio::fs::create_dir_all(&exports_dir).await {
        let err_msg = format!("Failed to create exports directory: {e}");
        error!(job_id = %job_id, error = %err_msg, "Export job failed");
        let _ = update_or_abort(
            repo,
            job_id,
            "failed",
            None,
            None,
            Some(&err_msg),
            worker_id,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        heartbeat_cancel.cancel();
        return;
    }

    let temp_path = attempt_temp_path(&exports_dir, job_id, worker_id, ext);
    let final_path = format!("{}/{}.{}", exports_dir, job_id, ext);

    // Stream the export into the attempt-scoped temp path. Lease-loss
    // cancellation propagates into the snapshot+write loop.
    let result = write_export_to_file(
        repo,
        job.dataset.as_sql_str(),
        format,
        target_id,
        network.as_deref(),
        time_start,
        time_end,
        &temp_path,
        lease_lost.clone(),
    )
    .await;

    // Heartbeat deliberately stays alive through publish + sink
    // delivery + final status update, so we don't release the lease
    // until side effects are durable.

    match result {
        Ok((record_count, _export_meta)) => {
            // Before touching the canonical download path, confirm we
            // still hold the lease. If we don't, the heartbeat task has
            // already set `lease_lost`; clean up the temp and bail
            // without racing the new owner.
            if lease_lost.is_cancelled() {
                warn!(
                    job_id = %job_id,
                    "Lease lost after write — discarding temp artifact without publishing"
                );
                best_effort_unlink(&temp_path, job_id).await;
                heartbeat_cancel.cancel();
                return;
            }

            let has_sink = job.sink_config.is_some();

            let prov_dv_id = _export_meta.dataset_version_id;
            let prov_dv = _export_meta.dataset_version;
            let prov_cs = _export_meta.completeness_status.as_deref();
            let prov_cc = _export_meta.completeness_coverage.as_ref();
            let prov_lri = _export_meta.last_ingestion_run_id;

            let result_location = format!("exports/{}.{}", job_id, ext);

            if has_sink {
                // Transition to 'delivering'. update_or_abort only
                // writes the row if we still own the lease (Ok(true)).
                // If Ok(false) ("someone else took over"), we clean up
                // our temp and exit without publishing.
                match update_or_abort(
                    repo,
                    job_id,
                    "delivering",
                    Some(record_count as i32),
                    Some(&result_location),
                    None,
                    worker_id,
                    None,
                    prov_dv_id,
                    prov_dv,
                    prov_cs,
                    prov_cc,
                    prov_lri,
                )
                .await
                {
                    Ok(()) => {}
                    Err(()) => {
                        best_effort_unlink(&temp_path, job_id).await;
                        heartbeat_cancel.cancel();
                        return;
                    }
                }

                // Publish: atomic rename temp -> final. Only after the
                // lease-confirming 'delivering' update above, so a
                // stale worker never publishes over the new owner's
                // artifact. rename is atomic within the same
                // filesystem; the exports_dir is a single dir so this
                // holds.
                if let Err(e) = tokio::fs::rename(&temp_path, &final_path).await {
                    let err_msg = format!("Failed to publish export artifact: {e}");
                    error!(job_id = %job_id, error = %err_msg, "Export job failed");
                    best_effort_unlink(&temp_path, job_id).await;
                    let _ = update_or_abort(
                        repo,
                        job_id,
                        "failed",
                        Some(record_count as i32),
                        Some(&result_location),
                        Some(&err_msg),
                        worker_id,
                        None,
                        prov_dv_id,
                        prov_dv,
                        prov_cs,
                        prov_cc,
                        prov_lri,
                    )
                    .await;
                    heartbeat_cancel.cancel();
                    return;
                }

                // Write provenance sidecar file alongside the export artifact.
                // Provenance is required for a complete export; failure fails the job.
                let prov_path = format!("{}/{}.provenance.json", exports_dir, job_id);
                if let Err(e) = write_provenance_file(
                    &prov_path,
                    job_id,
                    worker_id,
                    job.dataset.as_sql_str(),
                    format,
                    record_count,
                    prov_dv_id,
                    prov_dv,
                    prov_cs,
                    prov_cc,
                    prov_lri,
                )
                .await
                {
                    let err_msg = format!("Failed to write provenance sidecar: {e}");
                    error!(job_id = %job_id, error = %err_msg, "Export job failed");
                    if !lease_lost.is_cancelled() {
                        best_effort_unlink(&final_path, job_id).await;
                    }
                    let _ = update_or_abort(
                        repo,
                        job_id,
                        "failed",
                        Some(record_count as i32),
                        Some(&result_location),
                        Some(&err_msg),
                        worker_id,
                        None,
                        prov_dv_id,
                        prov_dv,
                        prov_cs,
                        prov_cc,
                        prov_lri,
                    )
                    .await;
                    heartbeat_cancel.cancel();
                    return;
                }

                // Guard sink delivery with a lease check: a reclaimed worker
                // must not deliver stale output to the sink.
                if lease_lost.is_cancelled() {
                    warn!(
                        job_id = %job_id,
                        "Lease lost before sink delivery — skipping delivery"
                    );
                    heartbeat_cancel.cancel();
                    return;
                }

                // Deliver to the sink from the already-streamed file.
                // `deliver_from_file` avoids loading the full artifact
                // into memory (fixes PR #238 [P2] sink-OOM concern):
                // LocalFileSink does a streaming `io::copy`; WebhookSink
                // uses `ReaderStream` for a streaming request body.
                let sink_value = job.sink_config.as_ref().unwrap();
                let delivery_result = match serde_json::from_value::<SinkConfig>(sink_value.clone())
                {
                    Ok(sink_config) => match build_sink(&sink_config, export_dir) {
                        Ok(sink) => {
                            let delivery_meta = DeliveryMetadata {
                                job_id,
                                dataset: job.dataset.to_string(),
                                format: job.format.to_string(),
                                record_count,
                                dataset_version_id: _export_meta.dataset_version_id,
                                completeness_status: _export_meta.completeness_status.clone(),
                            };
                            match sink
                                .deliver_from_file(
                                    std::path::Path::new(&final_path),
                                    &delivery_meta,
                                )
                                .await
                            {
                                Ok(receipt) => Ok(receipt.destination),
                                Err(e) => {
                                    warn!(job_id = %job_id, error = %e, "Sink delivery failed");
                                    Err(format!(
                                        "Exported {} records, but sink delivery failed: {e}",
                                        record_count
                                    ))
                                }
                            }
                        }
                        Err(e) => {
                            warn!(job_id = %job_id, error = %e, "Failed to build sink");
                            Err(format!(
                                "Exported {} records, but sink build failed: {e}",
                                record_count
                            ))
                        }
                    },
                    Err(e) => {
                        warn!(job_id = %job_id, error = %e, "Failed to parse sink config");
                        Err(format!(
                            "Exported {} records, but sink config parse failed: {e}",
                            record_count
                        ))
                    }
                };

                match delivery_result {
                    Ok(destination) => {
                        info!(
                            job_id = %job_id,
                            record_count,
                            destination = %destination,
                            "Export job completed with sink delivery"
                        );
                        let _ = update_or_abort(
                            repo,
                            job_id,
                            "completed",
                            Some(record_count as i32),
                            Some(&result_location),
                            None,
                            worker_id,
                            Some(&destination),
                            prov_dv_id,
                            prov_dv,
                            prov_cs,
                            prov_cc,
                            prov_lri,
                        )
                        .await;
                    }
                    Err(err_msg) => {
                        error!(job_id = %job_id, error = %err_msg, "Sink delivery failed");
                        let _ = update_or_abort(
                            repo,
                            job_id,
                            "failed",
                            Some(record_count as i32),
                            Some(&result_location),
                            Some(&err_msg),
                            worker_id,
                            None,
                            prov_dv_id,
                            prov_dv,
                            prov_cs,
                            prov_cc,
                            prov_lri,
                        )
                        .await;
                    }
                }
            } else {
                // No sink. Mirror the sink path's lease-guarded publish:
                // transition the row to `delivering` BEFORE the rename so
                // a stale worker that has been reclaimed cannot overwrite
                // the new owner's artifact. `update_or_abort` fails
                // closed (Err) when `worker_id = $7 AND status IN
                // ('running','delivering')` matches zero rows, i.e. the
                // lease is no longer ours.
                match update_or_abort(
                    repo,
                    job_id,
                    "delivering",
                    Some(record_count as i32),
                    Some(&result_location),
                    None,
                    worker_id,
                    None,
                    prov_dv_id,
                    prov_dv,
                    prov_cs,
                    prov_cc,
                    prov_lri,
                )
                .await
                {
                    Ok(()) => {}
                    Err(()) => {
                        best_effort_unlink(&temp_path, job_id).await;
                        heartbeat_cancel.cancel();
                        return;
                    }
                }

                // Publish: atomic rename temp -> final. Only runs after
                // the lease-confirming `delivering` update above, so a
                // stale worker never publishes over the new owner's
                // artifact.
                if let Err(e) = tokio::fs::rename(&temp_path, &final_path).await {
                    let err_msg = format!("Failed to publish export artifact: {e}");
                    error!(job_id = %job_id, error = %err_msg, "Export job failed");
                    best_effort_unlink(&temp_path, job_id).await;
                    let _ = update_or_abort(
                        repo,
                        job_id,
                        "failed",
                        Some(record_count as i32),
                        Some(&result_location),
                        Some(&err_msg),
                        worker_id,
                        None,
                        prov_dv_id,
                        prov_dv,
                        prov_cs,
                        prov_cc,
                        prov_lri,
                    )
                    .await;
                    heartbeat_cancel.cancel();
                    return;
                }

                // Write provenance sidecar file alongside the export artifact.
                // Provenance is required for a complete export; failure fails the job.
                let prov_path = format!("{}/{}.provenance.json", exports_dir, job_id);
                if let Err(e) = write_provenance_file(
                    &prov_path,
                    job_id,
                    worker_id,
                    job.dataset.as_sql_str(),
                    format,
                    record_count,
                    prov_dv_id,
                    prov_dv,
                    prov_cs,
                    prov_cc,
                    prov_lri,
                )
                .await
                {
                    let err_msg = format!("Failed to write provenance sidecar: {e}");
                    error!(job_id = %job_id, error = %err_msg, "Export job failed");
                    if !lease_lost.is_cancelled() {
                        best_effort_unlink(&final_path, job_id).await;
                    }
                    let _ = update_or_abort(
                        repo,
                        job_id,
                        "failed",
                        Some(record_count as i32),
                        Some(&result_location),
                        Some(&err_msg),
                        worker_id,
                        None,
                        prov_dv_id,
                        prov_dv,
                        prov_cs,
                        prov_cc,
                        prov_lri,
                    )
                    .await;
                    heartbeat_cancel.cancel();
                    return;
                }

                info!(
                    job_id = %job_id,
                    record_count,
                    result_location = %result_location,
                    "Export job completed"
                );
                match update_or_abort(
                    repo,
                    job_id,
                    "completed",
                    Some(record_count as i32),
                    Some(&result_location),
                    None,
                    worker_id,
                    None,
                    prov_dv_id,
                    prov_dv,
                    prov_cs,
                    prov_cc,
                    prov_lri,
                )
                .await
                {
                    Ok(()) => {}
                    Err(()) => {
                        // Lease was taken between the `delivering`
                        // transition and the `completed` transition
                        // (narrow window — our lease-holding update to
                        // `delivering` just succeeded). Do NOT unlink
                        // canonical paths here: a reclaiming worker may
                        // have already published new output, and deleting
                        // final_path/prov_path would race against it.
                        warn!(
                            job_id = %job_id,
                            "Lease lost at final update — leaving artifact/provenance for new owner"
                        );
                    }
                }
            }
        }
        Err(e) => {
            let err_msg = format!("Export failed: {e}");
            error!(job_id = %job_id, error = %err_msg, "Export job failed");
            // Attempt-scoped temp path: nothing was ever published at
            // final_path, so we only need to remove our temp.
            best_effort_unlink(&temp_path, job_id).await;
            let _ = update_or_abort(
                repo,
                job_id,
                "failed",
                None,
                None,
                Some(&err_msg),
                worker_id,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .await;
        }
    }

    // Stop heartbeat after all writes and status updates are done.
    heartbeat_cancel.cancel();
}

/// Wrapper around `update_export_job_status` that treats `Ok(false)` (lost
/// lease) the same as an error — logs a warning and returns `Err(())` so
/// the caller can abort immediately without continuing to execute side
/// effects on a reclaimed job.
#[allow(clippy::too_many_arguments)]
async fn update_or_abort(
    repo: &Repository,
    job_id: Uuid,
    status: &str,
    record_count: Option<i32>,
    result_location: Option<&str>,
    error_message: Option<&str>,
    worker_id: &str,
    delivery_destination: Option<&str>,
    dataset_version_id: Option<Uuid>,
    dataset_version: Option<i32>,
    completeness_status: Option<&str>,
    completeness_coverage: Option<&serde_json::Value>,
    last_ingestion_run_id: Option<Uuid>,
) -> Result<(), ()> {
    match repo
        .update_export_job_status(
            job_id,
            status,
            record_count,
            result_location,
            error_message,
            worker_id,
            delivery_destination,
            dataset_version_id,
            dataset_version,
            completeness_status,
            completeness_coverage,
            last_ingestion_run_id,
        )
        .await
    {
        Ok(true) => Ok(()),
        Ok(false) => {
            warn!(
                job_id = %job_id,
                status,
                "Lease lost — job was reclaimed by another worker; aborting"
            );
            Err(())
        }
        Err(e) => {
            error!(job_id = %job_id, error = %e, "Failed to update export job status");
            Err(())
        }
    }
}

/// Write a provenance sidecar JSON file next to the export artifact.
async fn write_provenance_file(
    path: &str,
    job_id: Uuid,
    worker_id: &str,
    dataset: &str,
    format: spectraplex_core::materializer::ExportFormat,
    record_count: usize,
    dataset_version_id: Option<Uuid>,
    dataset_version: Option<i32>,
    completeness_status: Option<&str>,
    completeness_coverage: Option<&serde_json::Value>,
    last_ingestion_run_id: Option<Uuid>,
) -> std::io::Result<()> {
    let prov = serde_json::json!({
        "export_job_id": job_id,
        "dataset": dataset,
        "format": format.to_string(),
        "record_count": record_count,
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "dataset_version_id": dataset_version_id,
        "dataset_version": dataset_version,
        "completeness_status": completeness_status,
        "completeness_coverage": completeness_coverage,
        "last_ingestion_run_id": last_ingestion_run_id,
    });

    // Atomic write with attempt-scoped temp path so concurrent/reclaimed
    // workers cannot clobber each other's sidecar temp.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    worker_id.hash(&mut h);
    let tag = format!("{:016x}", h.finish());
    let temp_path = format!("{}.{}.tmp", path, tag);

    let mut file = tokio::fs::File::create(&temp_path).await?;
    file.write_all(prov.to_string().as_bytes()).await?;
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&temp_path, path).await?;
    Ok(())
}

/// Parse the filters JSON value from an export job into individual query
/// parameters.
fn parse_filters(
    filters: &Option<serde_json::Value>,
) -> (Option<Uuid>, Option<String>, Option<i64>, Option<i64>) {
    let Some(filters) = filters else {
        return (None, None, None, None);
    };

    let target_id = filters
        .get("target_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Uuid>().ok());

    let network = filters
        .get("network")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let time_start = filters.get("time_start").and_then(|v| v.as_i64());

    let time_end = filters.get("time_end").and_then(|v| v.as_i64());

    (target_id, network, time_start, time_end)
}
