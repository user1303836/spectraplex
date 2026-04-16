//! Streaming export writer (fixes #208 export OOM).
//!
//! Replaces the previous "load the entire dataset into a `Vec<u8>` buffer,
//! then write it to disk" pattern with cursor-paginated streaming: records
//! are fetched from Postgres in bounded pages, each page is serialized into
//! a small per-page buffer, and each per-page buffer is streamed to disk
//! before the next page is fetched.
//!
//! Peak per-job memory is bounded by `(PAGE_SIZE * per-record bytes)`
//! regardless of total dataset size, so a single large export can no longer
//! OOM-kill the worker process and take down co-located workers.
//!
//! File opens use `O_NOFOLLOW` in the same way as [`crate::write_no_follow`]
//! (defense-in-depth for #220 TOCTOU symlink attacks) but the file handle is
//! kept open across the paged write loop rather than opened/closed per page.

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
use spectraplex_core::materializer::ExportFormat;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tracing::info;
use uuid::Uuid;

/// Records per paged query. Keeps peak per-job memory bounded to
/// roughly `PAGE_SIZE * sizeof(record)` regardless of total dataset size.
///
/// Chosen to match the "10,000 records per batch" suggestion from issue
/// #208 — large enough to amortize query round-trip cost, small enough that
/// a concurrent fleet of export workers cannot collectively OOM the host.
pub(crate) const PAGE_SIZE: i64 = 10_000;

/// Absolute safety cap on records per export job. The previous
/// `EXPORT_MAX_RECORDS = 100_000` cap in `v2_repo` is effectively replaced
/// by this cap (applied at the streaming layer, not the query layer).
///
/// Kept high enough that it does not become a surprise limit for real
/// exports but low enough that a misconfigured filter cannot accidentally
/// walk the entire dataset.
const EXPORT_HARD_CAP: i64 = 1_000_000;

/// BufWriter buffer capacity. Large enough that the writer does not flush
/// on every serialized record, small enough to stay well under the
/// PAGE_SIZE-bounded peak.
const WRITER_BUFFER_BYTES: usize = 256 * 1024;

/// Open the export target file with `O_NOFOLLOW` (mirrors
/// [`crate::write_no_follow`]'s symlink-refusing open) and return it as an
/// async [`tokio::fs::File`] suitable for streaming writes.
///
/// The underlying `std::fs::File::open` is performed on the blocking thread
/// pool so the async runtime is not blocked on filesystem syscalls.
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
///
/// Mirrors the metadata preamble that lived inside the old `run_export_job`
/// so the streaming path produces the same ExportJob row fields.
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

/// Encode a page of records as JSONL into `buf`.
fn encode_jsonl_page<T: Serialize>(records: &[T], buf: &mut Vec<u8>) -> anyhow::Result<()> {
    for r in records {
        serde_json::to_writer(&mut *buf, r)?;
        buf.push(b'\n');
    }
    Ok(())
}

/// Serialize a single page into an intermediate buffer, then stream it to
/// the async writer. Returns the number of bytes written.
async fn write_page<W, T, F>(
    writer: &mut BufWriter<W>,
    records: &[T],
    format: ExportFormat,
    csv_header: &str,
    write_header: bool,
    mut csv_row_writer: F,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
    F: FnMut(&[T], &mut Vec<u8>) -> std::io::Result<()>,
{
    // Buffer one page's bytes, then hand off to the async writer. The crucial
    // property is that we never accumulate more than one page's worth at a
    // time.
    let mut buf: Vec<u8> = Vec::with_capacity(records.len().saturating_mul(512));

    match format {
        ExportFormat::Jsonl => {
            encode_jsonl_page(records, &mut buf)?;
        }
        ExportFormat::Csv => {
            if write_header {
                std::io::Write::write_all(&mut buf, csv_header.as_bytes())?;
            }
            csv_row_writer(records, &mut buf)?;
        }
    }

    writer.write_all(&buf).await?;
    Ok(())
}

/// Generate one streaming-dispatch arm for a dataset. Expanded inside
/// [`write_export_to_file`] below.
macro_rules! stream_dataset {
    (
        $repo:expr, $writer:expr, $format:expr, $target_id:expr, $network:expr,
        $time_start:expr, $time_end:expr, $query_fn:ident,
        $csv_header_fn:path, $csv_rows_fn:path $(,)?
    ) => {{
        let mut offset: i64 = 0;
        let mut total: usize = 0;
        let mut write_header = true;
        loop {
            let records = $repo
                .$query_fn(
                    $target_id,
                    $network,
                    $time_start,
                    $time_end,
                    PAGE_SIZE,
                    offset,
                )
                .await?;
            let n = records.len();
            if n == 0 {
                break;
            }

            write_page(
                &mut $writer,
                &records,
                $format,
                $csv_header_fn(),
                write_header,
                |recs, buf| $csv_rows_fn(recs, buf),
            )
            .await?;

            write_header = false;
            total += n;
            offset += n as i64;

            // Short page means the underlying query is exhausted.
            if (n as i64) < PAGE_SIZE {
                break;
            }
            if offset >= EXPORT_HARD_CAP {
                break;
            }
        }
        total
    }};
}

/// Stream an export for `dataset` to `output_path`, fetching records in
/// pages of [`PAGE_SIZE`] and writing each page directly to disk.
///
/// Returns the total number of records written and the provenance metadata
/// gathered at the start of the export (matches the old `run_export_job`
/// contract minus the giant `Vec<u8>`).
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
) -> anyhow::Result<(usize, ExportMetadata)> {
    let meta = fetch_export_metadata(repo, dataset, target_id, network).await;

    let file = open_no_follow(output_path).await?;
    let mut writer = BufWriter::with_capacity(WRITER_BUFFER_BYTES, file);

    let total = match dataset {
        "token_transfers" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_token_transfers,
            token_transfers_csv_header,
            write_token_transfers_csv_rows,
        ),
        "native_balance_deltas" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_native_balance_deltas,
            native_balance_deltas_csv_header,
            write_native_balance_deltas_csv_rows,
        ),
        "decoded_events" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_decoded_events,
            decoded_events_csv_header,
            write_decoded_events_csv_rows,
        ),
        "hl_fills" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_hl_fill_records,
            hl_fills_csv_header,
            write_hl_fills_csv_rows,
        ),
        "hl_funding" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_hl_funding_payments,
            hl_funding_csv_header,
            write_hl_funding_csv_rows,
        ),
        "positions" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_hl_position_changes,
            hl_positions_csv_header,
            write_hl_positions_csv_rows,
        ),
        "wallet_ledger" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_wallet_ledger_records,
            wallet_ledger_csv_header,
            write_wallet_ledger_csv_rows,
        ),
        "balance_history" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_balance_snapshots,
            balance_history_csv_header,
            write_balance_history_csv_rows,
        ),
        "hl_pnl_summary" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_hl_pnl_summary,
            hl_pnl_summary_csv_header,
            write_hl_pnl_summary_csv_rows,
        ),
        "hl_trade_history" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_hl_trade_history,
            hl_trade_history_csv_header,
            write_hl_trade_history_csv_rows,
        ),
        "protocol_events" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_protocol_events,
            protocol_events_csv_header,
            write_protocol_events_csv_rows,
        ),
        "pool_snapshots" => stream_dataset!(
            repo,
            writer,
            format,
            target_id,
            network,
            time_start,
            time_end,
            query_pool_snapshots,
            pool_snapshots_csv_header,
            write_pool_snapshots_csv_rows,
        ),
        other => return Err(anyhow::anyhow!("Unknown dataset: {other}")),
    };

    writer.flush().await?;

    info!(
        dataset,
        record_count = total,
        output_path,
        "Export streamed to disk"
    );

    Ok((total, meta))
}
