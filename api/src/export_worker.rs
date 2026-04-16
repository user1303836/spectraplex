//! Durable export worker loop.
//!
//! Claims pending `export_jobs` rows from Postgres, executes the actual
//! export via [`crate::run_export_job`], optionally delivers to a configured
//! sink, and marks jobs complete/failed durably. Heartbeats while running so
//! that dead workers are detected and their jobs reclaimed.

use std::sync::Arc;
use std::time::Duration;

use spectraplex_adapters::repo::Repository;
use spectraplex_core::config::AppConfig;
use spectraplex_core::materializer::{DeliveryMetadata, ExportFormat, SinkConfig};
use spectraplex_core::v2::ExportJob;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

// These items are defined in main.rs and need to be pub(crate) for this
// module to use them.  The parent task will adjust visibility.
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
    // Try to acquire a semaphore permit before claiming a job.
    // This prevents the worker from claiming more jobs than it can execute
    // concurrently.
    let permit = match Arc::clone(job_semaphore).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            // All permits are in use — back off and retry.
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
            // No jobs available — drop the permit and sleep before polling again.
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

async fn execute_export_job(
    repo: &Repository,
    config: &AppConfig,
    worker_id: &str,
    job: &ExportJob,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    let job_id = job.id;
    let export_dir = &config.export_dir;

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
                    match hb_repo.heartbeat_export_job(hb_job_id, &hb_worker).await {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!(job_id = %hb_job_id, "Heartbeat returned false — lease lost");
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

    // Parse filters from the job to extract query parameters.
    let (target_id, network, time_start, time_end) = parse_filters(&job.filters);

    // Parse the export format.
    let format: ExportFormat = match job.format.parse() {
        Ok(f) => f,
        Err(_) => {
            heartbeat_cancel.cancel();
            let err_msg = format!("Invalid export format: {}", job.format);
            error!(job_id = %job_id, error = %err_msg, "Export job failed");
            if let Err(e) = repo
                .update_export_job_status(
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
                .await
            {
                error!(job_id = %job_id, error = %e, "Failed to mark job failed");
            }
            return;
        }
    };

    // Determine file extension from format, so we can build the artifact
    // path before running the export. The path is needed up-front because
    // `write_export_to_file` streams records directly to this path, page by
    // page (fix for #208: the old code accumulated the whole export in
    // memory first).
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

    let file_path = format!("{}/{}.{}", exports_dir, job_id, ext);

    // Stream the export directly to disk. Peak per-job memory is bounded by
    // `export_stream::PAGE_SIZE * per-record bytes`, not by total dataset
    // size.
    let result = write_export_to_file(
        repo,
        &job.dataset,
        format,
        target_id,
        network.as_deref(),
        time_start,
        time_end,
        &file_path,
    )
    .await;

    // NOTE: Heartbeat stays alive through file write, sink delivery, and
    // final status update. Stopping it early would let claim_export_job()
    // reclaim a "delivering" job that is still actively writing/delivering,
    // causing duplicate deliveries and conflicting final states.

    match result {
        Ok((record_count, _export_meta)) => {
            let has_sink = job.sink_config.is_some();

            // Extract provenance fields from export metadata for persistence.
            let prov_dv_id = _export_meta.dataset_version_id;
            let prov_dv = _export_meta.dataset_version;
            let prov_cs = _export_meta.completeness_status.as_deref();
            let prov_cc = _export_meta.completeness_coverage.as_ref();
            let prov_lri = _export_meta.last_ingestion_run_id;

            // Canonical on-disk artifact path — always preserved for download.
            let result_location = format!("exports/{}.{}", job_id, ext);

            if has_sink {
                // Transition to 'delivering'. If Ok(false), lease was lost.
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
                        heartbeat_cancel.cancel();
                        return;
                    }
                }

                // Attempt sink delivery.
                //
                // The streaming writer already produced the export artifact
                // on disk. The current `ExportSink::deliver` contract takes
                // `&[u8]`, so we read the artifact back into memory once for
                // the sink call. Peak memory here is bounded by the disk
                // artifact size (which is itself bounded by
                // `export_stream::EXPORT_HARD_CAP * per-record bytes`) — much
                // smaller than the pre-#208 behavior where the full export
                // was also held in memory for the disk write. Converting the
                // sink trait to take a file path / reader is a follow-up.
                let sink_value = job.sink_config.as_ref().unwrap();
                let delivery_result = match serde_json::from_value::<SinkConfig>(sink_value.clone())
                {
                    Ok(sink_config) => match build_sink(&sink_config, export_dir) {
                        Ok(sink) => match tokio::fs::read(&file_path).await {
                            Ok(body) => {
                                let delivery_meta = DeliveryMetadata {
                                    job_id,
                                    dataset: job.dataset.clone(),
                                    format: job.format.clone(),
                                    record_count,
                                    dataset_version_id: _export_meta.dataset_version_id,
                                    completeness_status: _export_meta.completeness_status.clone(),
                                };
                                match sink.deliver(&body, &delivery_meta).await {
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
                                warn!(
                                    job_id = %job_id,
                                    error = %e,
                                    file = %file_path,
                                    "Failed to read export artifact for sink delivery",
                                );
                                Err(format!(
                                    "Exported {} records to disk, but reading artifact for sink delivery failed: {e}",
                                    record_count
                                ))
                            }
                        },
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
                        // Keep result_location as the on-disk file path for download.
                        // The sink destination is stored in delivery_destination.
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
                // No sink — mark completed directly with the filesystem location.
                info!(
                    job_id = %job_id,
                    record_count,
                    result_location = %result_location,
                    "Export job completed"
                );
                let _ = update_or_abort(
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
                .await;
            }
        }
        Err(e) => {
            let err_msg = format!("Export failed: {e}");
            error!(job_id = %job_id, error = %err_msg, "Export job failed");
            // The streaming writer may have created a partial artifact before
            // failing. Best-effort remove it so a retry does not append to a
            // truncated file and so we do not hand out a partial artifact via
            // `/download`.
            if let Err(rm_err) = tokio::fs::remove_file(&file_path).await {
                if rm_err.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        job_id = %job_id,
                        error = %rm_err,
                        file = %file_path,
                        "Failed to clean up partial export artifact after failure",
                    );
                }
            }
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
