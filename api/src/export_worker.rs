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
use crate::{build_sink, run_export_job, ExportMetadata};

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
                .update_export_job_status(job_id, "failed", None, None, Some(&err_msg), worker_id)
                .await
            {
                error!(job_id = %job_id, error = %e, "Failed to mark job failed");
            }
            return;
        }
    };

    // Execute the actual export.
    let result = run_export_job(
        repo,
        &job.dataset,
        format,
        target_id,
        network.as_deref(),
        time_start,
        time_end,
    )
    .await;

    // Stop heartbeat.
    heartbeat_cancel.cancel();

    match result {
        Ok((body, record_count, export_meta)) => {
            let has_sink = job.sink_config.is_some();

            // Determine file extension from format.
            let ext = match format {
                ExportFormat::Jsonl => "jsonl",
                ExportFormat::Csv => "csv",
            };

            // Write export data to filesystem.
            let exports_dir = format!("{}/exports", export_dir);
            if let Err(e) = tokio::fs::create_dir_all(&exports_dir).await {
                let err_msg = format!("Failed to create exports directory: {e}");
                error!(job_id = %job_id, error = %err_msg, "Export job failed");
                let _ = repo
                    .update_export_job_status(
                        job_id,
                        "failed",
                        None,
                        None,
                        Some(&err_msg),
                        worker_id,
                    )
                    .await;
                return;
            }

            let file_path = format!("{}/{}.{}", exports_dir, job_id, ext);
            if let Err(e) = tokio::fs::write(&file_path, &body).await {
                let err_msg = format!("Failed to write export file: {e}");
                error!(job_id = %job_id, error = %err_msg, "Export job failed");
                let _ = repo
                    .update_export_job_status(
                        job_id,
                        "failed",
                        None,
                        None,
                        Some(&err_msg),
                        worker_id,
                    )
                    .await;
                return;
            }

            let result_location = format!("exports/{}.{}", job_id, ext);

            if has_sink {
                // Transition to 'delivering' state before attempting sink delivery.
                if let Err(e) = repo
                    .update_export_job_status(
                        job_id,
                        "delivering",
                        Some(record_count as i32),
                        Some(&result_location),
                        None,
                        worker_id,
                    )
                    .await
                {
                    error!(job_id = %job_id, error = %e, "Failed to transition to delivering");
                    return;
                }

                // Attempt sink delivery.
                let sink_value = job.sink_config.as_ref().unwrap();
                let delivery_result = match serde_json::from_value::<SinkConfig>(sink_value.clone())
                {
                    Ok(sink_config) => match build_sink(&sink_config, export_dir) {
                        Ok(sink) => {
                            let delivery_meta = DeliveryMetadata {
                                job_id,
                                dataset: job.dataset.clone(),
                                format: job.format.clone(),
                                record_count,
                                dataset_version_id: export_meta.dataset_version_id,
                                completeness_status: export_meta.completeness_status,
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
                        if let Err(e) = repo
                            .update_export_job_status(
                                job_id,
                                "completed",
                                Some(record_count as i32),
                                Some(&destination),
                                None,
                                worker_id,
                            )
                            .await
                        {
                            error!(job_id = %job_id, error = %e, "Failed to mark job completed");
                        }
                    }
                    Err(err_msg) => {
                        error!(job_id = %job_id, error = %err_msg, "Sink delivery failed");
                        if let Err(e) = repo
                            .update_export_job_status(
                                job_id,
                                "failed",
                                Some(record_count as i32),
                                Some(&result_location),
                                Some(&err_msg),
                                worker_id,
                            )
                            .await
                        {
                            error!(job_id = %job_id, error = %e, "Failed to mark job failed");
                        }
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
                if let Err(e) = repo
                    .update_export_job_status(
                        job_id,
                        "completed",
                        Some(record_count as i32),
                        Some(&result_location),
                        None,
                        worker_id,
                    )
                    .await
                {
                    error!(job_id = %job_id, error = %e, "Failed to mark job completed");
                }
            }
        }
        Err(e) => {
            let err_msg = format!("Export failed: {e}");
            error!(job_id = %job_id, error = %err_msg, "Export job failed");
            if let Err(e2) = repo
                .update_export_job_status(job_id, "failed", None, None, Some(&err_msg), worker_id)
                .await
            {
                error!(job_id = %job_id, error = %e2, "Failed to mark job failed");
            }
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
