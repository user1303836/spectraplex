//! Streaming export writer (fixes #208 export OOM).
//!
//! Replaces the previous "load the entire dataset into a `Vec<u8>` buffer,
//! then write it to disk" pattern with cursor-paginated streaming inside
//! a `REPEATABLE READ READ ONLY` Postgres transaction. Records are
//! fetched in bounded pages, each page is serialized into a small
//! per-page buffer, and each per-page buffer is streamed to disk before
//! the next page is fetched.
//!
//! Snapshot-stable: pagination runs inside a single snapshot
//! transaction, and the dataset `ORDER BY` carries a `dt.id DESC`
//! tiebreaker so concurrent ingestion cannot cause page boundaries to
//! skip or duplicate rows (addresses PR #238 [P1] pagination concern).
//!
//! Cancellable: the caller supplies a [`CancellationToken`] that the
//! worker drives from the heartbeat task — when the export job's lease
//! is reclaimed by another worker, the token is cancelled, and this
//! function aborts cleanly without emitting more bytes. The caller is
//! responsible for cleaning up any attempt-scoped temp artifact before
//! publishing to the final download path.
//!
//! Peak per-job memory is bounded by `PAGE_SIZE * per-record bytes`
//! regardless of total dataset size, so a single large export can no
//! longer OOM-kill the worker process and take down co-located workers.
//!
//! File opens use `O_NOFOLLOW` in the same way as
//! [`crate::write_no_follow`] (defense-in-depth for #220 TOCTOU symlink
//! attacks) but the file handle is kept open across the paged write
//! loop rather than opened/closed per page.

use crate::export_csv::{
    balance_history_csv_header, decoded_events_csv_header, hl_fills_csv_header,
    hl_funding_csv_header, hl_pnl_summary_csv_header, hl_positions_csv_header,
    hl_trade_history_csv_header, native_balance_deltas_csv_header, pool_snapshots_csv_header,
    protocol_events_csv_header, token_transfers_csv_header, wallet_ledger_csv_header,
    write_balance_history_csv_rows, write_decoded_events_csv_rows, write_hl_fills_csv_rows,
    write_hl_funding_csv_rows, write_hl_pnl_summary_csv_rows, write_hl_positions_csv_rows,
    write_hl_trade_history_csv_rows, write_native_balance_deltas_csv_rows,
    write_pool_snapshots_csv_rows, write_protocol_events_csv_rows, write_token_transfers_csv_rows,
    write_wallet_ledger_csv_rows,
};
use crate::ExportMetadata;
use serde::Serialize;
use spectraplex_adapters::repo::Repository;
use spectraplex_adapters::v2_repo::ExportRecordBatch;
use spectraplex_core::materializer::ExportFormat;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;
use tracing::info;
use uuid::Uuid;

/// Records per paged query. Keeps peak per-job memory bounded to
/// roughly `PAGE_SIZE * sizeof(record)` regardless of total dataset size.
///
/// Chosen to match the "10,000 records per batch" suggestion from issue
/// #208 — large enough to amortize query round-trip cost, small enough that
/// a concurrent fleet of export workers cannot collectively OOM the host.
pub(crate) const PAGE_SIZE: i64 = 10_000;

/// Absolute safety cap on records per export job.
const EXPORT_HARD_CAP: i64 = 1_000_000;

/// BufWriter buffer capacity. Large enough that the writer does not flush
/// on every serialized record, small enough to stay well under the
/// PAGE_SIZE-bounded peak.
const WRITER_BUFFER_BYTES: usize = 256 * 1024;

/// Channel depth for page handoff between the DB snapshot task and the
/// disk writer. Kept small so DB streaming throttles to disk speed
/// rather than buffering whole pages in RAM.
const PAGE_CHANNEL_CAPACITY: usize = 2;

/// Open the export target file with `O_NOFOLLOW` (mirrors
/// [`crate::write_no_follow`]'s symlink-refusing open) and return it as an
/// async [`tokio::fs::File`] suitable for streaming writes.
async fn open_no_follow(path: &str) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let path_owned = path.to_owned();
    let std_file = tokio::task::spawn_blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path_owned)
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(File::from_std(std_file))
}

/// Look up dataset-version and completeness provenance for an export job.
pub(crate) async fn fetch_export_metadata(
    repo: &Repository,
    dataset: &str,
    target_id: Option<Uuid>,
    network: Option<&str>,
) -> ExportMetadata {
    let active_version = repo
        .get_active_dataset_version(dataset)
        .await
        .ok()
        .flatten();

    let completeness_records = repo
        .list_completeness_filtered(dataset, target_id, network)
        .await
        .unwrap_or_default();

    let mut meta = ExportMetadata::default();
    if let Some(ref dv) = active_version {
        meta.dataset_version_id = Some(dv.id);
        meta.dataset_version = Some(dv.version);
    }

    if !completeness_records.is_empty() {
        let statuses: Vec<&str> = completeness_records
            .iter()
            .map(|c| match c.status {
                spectraplex_core::v2::CompletenessStatus::Complete => "complete",
                spectraplex_core::v2::CompletenessStatus::Partial => "partial",
                spectraplex_core::v2::CompletenessStatus::Backfilling => "backfilling",
                spectraplex_core::v2::CompletenessStatus::Gap => "gap",
            })
            .collect();

        let status = if statuses.contains(&"gap") {
            "gap"
        } else if statuses.contains(&"backfilling") {
            "backfilling"
        } else if statuses.contains(&"partial") {
            "partial"
        } else {
            "complete"
        };
        meta.completeness_status = Some(status.to_string());

        let coverage_start = completeness_records
            .iter()
            .filter_map(|c| c.coverage_start)
            .min();
        let coverage_end = completeness_records
            .iter()
            .filter_map(|c| c.coverage_end)
            .max();
        let block_start = completeness_records
            .iter()
            .filter_map(|c| c.block_start)
            .min();
        let block_end = completeness_records
            .iter()
            .filter_map(|c| c.block_end)
            .max();
        meta.completeness_coverage = Some(serde_json::json!({
            "coverage_start": coverage_start,
            "coverage_end": coverage_end,
            "block_start": block_start,
            "block_end": block_end,
        }));

        meta.last_ingestion_run_id = completeness_records
            .iter()
            .rev()
            .find_map(|c| c.last_ingestion_run_id);
    }

    meta
}

/// Return the canonical CSV header line for a given dataset name, or
/// `None` if the dataset is unknown.
fn csv_header_for(dataset: &str) -> Option<&'static str> {
    Some(match dataset {
        "token_transfers" => token_transfers_csv_header(),
        "native_balance_deltas" => native_balance_deltas_csv_header(),
        "decoded_events" => decoded_events_csv_header(),
        "hl_fills" => hl_fills_csv_header(),
        "hl_funding" => hl_funding_csv_header(),
        "positions" => hl_positions_csv_header(),
        "wallet_ledger" => wallet_ledger_csv_header(),
        "balance_history" => balance_history_csv_header(),
        "hl_pnl_summary" => hl_pnl_summary_csv_header(),
        "hl_trade_history" => hl_trade_history_csv_header(),
        "protocol_events" => protocol_events_csv_header(),
        "pool_snapshots" => pool_snapshots_csv_header(),
        _ => return None,
    })
}

/// Encode a page of records as JSONL into `buf`.
fn encode_jsonl_page<T: Serialize>(records: &[T], buf: &mut Vec<u8>) -> anyhow::Result<()> {
    for r in records {
        serde_json::to_writer(&mut *buf, r)?;
        buf.push(b'\n');
    }
    Ok(())
}

/// Serialize one batch (a page returned by the snapshot streamer) into
/// the caller-provided byte buffer. CSV rows only — the CSV header is
/// emitted once before the loop by [`write_export_to_file`].
fn encode_batch(
    batch: &ExportRecordBatch,
    format: ExportFormat,
    buf: &mut Vec<u8>,
) -> anyhow::Result<()> {
    match (format, batch) {
        (ExportFormat::Jsonl, ExportRecordBatch::TokenTransfers(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::NativeBalanceDeltas(v)) => {
            encode_jsonl_page(v, buf)
        }
        (ExportFormat::Jsonl, ExportRecordBatch::DecodedEvents(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::HlFills(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::HlFunding(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::Positions(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::WalletLedger(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::BalanceHistory(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::HlPnlSummary(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::HlTradeHistory(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::ProtocolEvents(v)) => encode_jsonl_page(v, buf),
        (ExportFormat::Jsonl, ExportRecordBatch::PoolSnapshots(v)) => encode_jsonl_page(v, buf),

        (ExportFormat::Csv, ExportRecordBatch::TokenTransfers(v)) => {
            Ok(write_token_transfers_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::NativeBalanceDeltas(v)) => {
            Ok(write_native_balance_deltas_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::DecodedEvents(v)) => {
            Ok(write_decoded_events_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::HlFills(v)) => Ok(write_hl_fills_csv_rows(v, buf)?),
        (ExportFormat::Csv, ExportRecordBatch::HlFunding(v)) => {
            Ok(write_hl_funding_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::Positions(v)) => {
            Ok(write_hl_positions_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::WalletLedger(v)) => {
            Ok(write_wallet_ledger_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::BalanceHistory(v)) => {
            Ok(write_balance_history_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::HlPnlSummary(v)) => {
            Ok(write_hl_pnl_summary_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::HlTradeHistory(v)) => {
            Ok(write_hl_trade_history_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::ProtocolEvents(v)) => {
            Ok(write_protocol_events_csv_rows(v, buf)?)
        }
        (ExportFormat::Csv, ExportRecordBatch::PoolSnapshots(v)) => {
            Ok(write_pool_snapshots_csv_rows(v, buf)?)
        }
    }
}

/// Stream an export for `dataset` to `output_path`, fetching records
/// in pages of [`PAGE_SIZE`] inside a `REPEATABLE READ READ ONLY`
/// transaction and writing each page directly to disk.
///
/// Returns the total number of records written and the provenance
/// metadata gathered at the start of the export.
///
/// # Cancellation
///
/// The `cancel` token is checked before the write loop pulls each
/// page from the channel and before each async write. When cancelled,
/// the snapshot task aborts the remaining pages, the transaction is
/// rolled back implicitly on drop, and this function returns an error.
/// The caller is responsible for deleting the (attempt-scoped) output
/// file so a reclaiming worker never sees partial bytes.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_export_to_file(
    repo: &Repository,
    dataset: &str,
    format: ExportFormat,
    target_id: Option<Uuid>,
    network: Option<&str>,
    time_start: Option<i64>,
    time_end: Option<i64>,
    output_path: &str,
    cancel: CancellationToken,
) -> anyhow::Result<(usize, ExportMetadata)> {
    let meta = fetch_export_metadata(repo, dataset, target_id, network).await;

    // Reject unknown datasets before opening the file so we don't leave
    // an empty artifact behind.
    let csv_header =
        csv_header_for(dataset).ok_or_else(|| anyhow::anyhow!("Unknown dataset: {dataset}"))?;

    let file = open_no_follow(output_path).await?;
    let mut writer = BufWriter::with_capacity(WRITER_BUFFER_BYTES, file);

    // Always emit the CSV header before the page loop so an export with
    // zero rows still produces a header-only file (fixes PR #238 [P2]
    // empty-CSV regression). For JSONL, zero rows correctly produces an
    // empty file.
    if matches!(format, ExportFormat::Csv) {
        writer.write_all(csv_header.as_bytes()).await?;
    }

    // Spawn the snapshot streamer on a separate task so the snapshot
    // transaction and the write loop run concurrently, with backpressure
    // provided by the small channel.
    let (page_tx, mut page_rx) =
        tokio::sync::mpsc::channel::<ExportRecordBatch>(PAGE_CHANNEL_CAPACITY);
    let repo_clone = repo.clone();
    let dataset_owned = dataset.to_string();
    let network_owned = network.map(|s| s.to_string());
    let cancel_for_stream = cancel.clone();

    let stream_task = tokio::spawn(async move {
        repo_clone
            .stream_export_snapshot(
                &dataset_owned,
                target_id,
                network_owned.as_deref(),
                time_start,
                time_end,
                PAGE_SIZE,
                EXPORT_HARD_CAP,
                cancel_for_stream,
                page_tx,
            )
            .await
    });

    let mut total: usize = 0;
    let write_result: anyhow::Result<()> = async {
        while let Some(batch) = page_rx.recv().await {
            if cancel.is_cancelled() {
                anyhow::bail!("export cancelled during write loop");
            }

            let n = batch.len();
            let mut buf: Vec<u8> = Vec::with_capacity(n.saturating_mul(512));
            encode_batch(&batch, format, &mut buf)?;
            writer.write_all(&buf).await?;
            total += n;
        }
        Ok(())
    }
    .await;

    // Always drain the snapshot task so we propagate its errors (e.g.
    // lease-lost cancellation, DB errors) and don't leak a pending
    // transaction. If the write loop failed, cancel the token so the
    // snapshot task short-circuits rather than continuing to push
    // pages into a dropped channel.
    if write_result.is_err() {
        cancel.cancel();
    }

    let stream_result = stream_task
        .await
        .map_err(|e| anyhow::anyhow!("export snapshot task panicked: {e}"))?;

    // Surface the first error we have. The write loop's error is
    // generally the root cause (cancellation, disk failure); the
    // snapshot task typically reports the secondary "channel dropped".
    write_result?;
    let streamed = stream_result?;

    // Sanity check: the DB-reported total must match what we wrote.
    // A mismatch would indicate dropped pages or a bookkeeping bug.
    debug_assert_eq!(streamed, total);

    writer.flush().await?;

    info!(
        dataset,
        record_count = total,
        output_path,
        "Export streamed to disk"
    );

    Ok((total, meta))
}
