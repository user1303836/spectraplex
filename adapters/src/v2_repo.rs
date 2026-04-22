//! V2 repository methods for canonical bronze data and control-plane tables.
//!
//! All methods are added as `impl Repository` so callers use the same
//! `Repository` value they already hold for V1 wallet-scoped queries.

use chrono::{DateTime, Utc};

/// V2-backed wallet statistics returned by `get_wallet_stats_v2`.
pub struct WalletStatsV2 {
    pub tx_count: i64,
    pub earliest_timestamp: Option<i64>,
    pub latest_timestamp: Option<i64>,
    pub network_count: i64,
    pub unique_assets: i64,
    pub per_network: Vec<(String, i64)>,
}
use spectraplex_core::materializer::{
    BalanceSnapshot, DatasetName, DecodedEvent, ExportFormat, HlFillRecord, HlFundingPayment,
    HlPnlSummary, HlPositionChange, HlTradeHistory, NativeBalanceDelta, PoolSnapshot,
    ProtocolEvent, TokenTransfer, WalletLedgerRecord,
};
use spectraplex_core::v2::{
    ApiKey, ChainFamily, Checkpoint, CompletenessStatus, DatasetCompleteness, DatasetVersion,
    DatasetVersionStatus, DatasetWatermark, EvmTraceType, ExportJob, ExportJobStatus, IndexTarget,
    IngestionJob, IngestionJobMode, IngestionJobStatus, IngestionRun, MaterializationRun,
    MaterializationRunStatus, Network, RawEvmTrace, RawTransaction, StreamActualStatus,
    StreamDesiredStatus, StreamSource, StreamSubscription, TargetKind, TargetMatch, TargetMode,
};
use sqlx::Row;
use uuid::Uuid;

use crate::repo::Repository;

/// Maximum allowed LIMIT for V2 repository queries to prevent unbounded result sets.
const MAX_QUERY_LIMIT: i64 = 10_000;

// ---------------------------------------------------------------------------
// Enum ↔ SQL string helpers
// ---------------------------------------------------------------------------

/// Convert a `ChainFamily` to the string used by `chain_family_enum` in SQL.
pub fn chain_family_to_sql(cf: &ChainFamily) -> &'static str {
    match cf {
        ChainFamily::Solana => "solana",
        ChainFamily::Evm => "evm",
        ChainFamily::Hyperliquid => "hyperliquid",
    }
}

/// Parse a SQL `chain_family_enum` string back to `ChainFamily`.
pub fn sql_to_chain_family(s: &str) -> anyhow::Result<ChainFamily> {
    match s {
        "solana" => Ok(ChainFamily::Solana),
        "evm" => Ok(ChainFamily::Evm),
        "hyperliquid" => Ok(ChainFamily::Hyperliquid),
        _ => Err(anyhow::anyhow!("Unknown chain_family: {s}")),
    }
}

/// Convert a `TargetKind` to the string used by `target_kind_enum` in SQL.
pub fn target_kind_to_sql(tk: &TargetKind) -> &'static str {
    match tk {
        TargetKind::Wallet => "wallet",
        TargetKind::Contract => "contract",
        TargetKind::Program => "program",
        TargetKind::Account => "account",
        TargetKind::TopicFilter => "topic_filter",
        TargetKind::Market => "market",
        TargetKind::Pool => "pool",
        TargetKind::Protocol => "protocol",
    }
}

/// Parse a SQL `target_kind_enum` string back to `TargetKind`.
pub fn sql_to_target_kind(s: &str) -> anyhow::Result<TargetKind> {
    match s {
        "wallet" => Ok(TargetKind::Wallet),
        "contract" => Ok(TargetKind::Contract),
        "program" => Ok(TargetKind::Program),
        "account" => Ok(TargetKind::Account),
        "topic_filter" => Ok(TargetKind::TopicFilter),
        "market" => Ok(TargetKind::Market),
        "pool" => Ok(TargetKind::Pool),
        "protocol" => Ok(TargetKind::Protocol),
        _ => Err(anyhow::anyhow!("Unknown target_kind: {s}")),
    }
}

/// Convert a `TargetMode` to the string used by `target_mode_enum` in SQL.
pub fn target_mode_to_sql(tm: &TargetMode) -> &'static str {
    match tm {
        TargetMode::Backfill => "backfill",
        TargetMode::Stream => "stream",
        TargetMode::Both => "both",
    }
}

/// Parse a SQL `target_mode_enum` string back to `TargetMode`.
pub fn sql_to_target_mode(s: &str) -> anyhow::Result<TargetMode> {
    match s {
        "backfill" => Ok(TargetMode::Backfill),
        "stream" => Ok(TargetMode::Stream),
        "both" => Ok(TargetMode::Both),
        _ => Err(anyhow::anyhow!("Unknown target_mode: {s}")),
    }
}

/// Convert a `DatasetVersionStatus` to the string used in SQL.
pub fn dataset_version_status_to_sql(s: &DatasetVersionStatus) -> &'static str {
    match s {
        DatasetVersionStatus::Active => "active",
        DatasetVersionStatus::Superseded => "superseded",
        DatasetVersionStatus::Failed => "failed",
    }
}

/// Parse a SQL status string back to `DatasetVersionStatus`.
pub fn sql_to_dataset_version_status(s: &str) -> anyhow::Result<DatasetVersionStatus> {
    match s {
        "active" => Ok(DatasetVersionStatus::Active),
        "superseded" => Ok(DatasetVersionStatus::Superseded),
        "failed" => Ok(DatasetVersionStatus::Failed),
        _ => Err(anyhow::anyhow!("Unknown dataset_version_status: {s}")),
    }
}

/// Convert a `CompletenessStatus` to the string stored in SQL.
pub fn completeness_status_to_sql(s: &CompletenessStatus) -> &'static str {
    match s {
        CompletenessStatus::Partial => "partial",
        CompletenessStatus::Complete => "complete",
        CompletenessStatus::Backfilling => "backfilling",
        CompletenessStatus::Gap => "gap",
    }
}

/// Parse a SQL completeness status string back to `CompletenessStatus`.
pub fn sql_to_completeness_status(s: &str) -> anyhow::Result<CompletenessStatus> {
    match s {
        "partial" => Ok(CompletenessStatus::Partial),
        "complete" => Ok(CompletenessStatus::Complete),
        "backfilling" => Ok(CompletenessStatus::Backfilling),
        "gap" => Ok(CompletenessStatus::Gap),
        _ => Err(anyhow::anyhow!("Unknown completeness_status: {s}")),
    }
}

// ---------------------------------------------------------------------------
// Row-mapping helpers
// ---------------------------------------------------------------------------

fn row_to_network(row: &sqlx::postgres::PgRow) -> anyhow::Result<Network> {
    let family_str: String = row.try_get("chain_family")?;
    Ok(Network {
        id: row.try_get("id")?,
        chain_family: sql_to_chain_family(&family_str)?,
        display_name: row.try_get("display_name")?,
        is_testnet: row.try_get("is_testnet")?,
        finality_model: {
            let s: String = row.try_get("finality_model")?;
            s.parse()
                .map_err(|_| anyhow::anyhow!("bad finality_model: {s}"))?
        },
        block_time_ms: row.try_get("block_time_ms")?,
    })
}

fn row_to_index_target(row: &sqlx::postgres::PgRow) -> anyhow::Result<IndexTarget> {
    let kind_str: String = row.try_get("kind")?;
    let family_str: String = row.try_get("chain_family")?;
    let mode_str: String = row.try_get("mode")?;
    Ok(IndexTarget {
        id: row.try_get("id")?,
        kind: sql_to_target_kind(&kind_str)?,
        network: row.try_get("network")?,
        chain_family: sql_to_chain_family(&family_str)?,
        address: row.try_get("address")?,
        filter_spec: row.try_get("filter_spec")?,
        mode: sql_to_target_mode(&mode_str)?,
        label: row.try_get("label")?,
        owner_id: row.try_get("owner_id")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_raw_transaction(row: &sqlx::postgres::PgRow) -> anyhow::Result<RawTransaction> {
    Ok(RawTransaction {
        id: row.try_get("id")?,
        network: row.try_get("network")?,
        tx_hash: row.try_get("tx_hash")?,
        timestamp: row.try_get("timestamp")?,
        block_number: row.try_get("block_number")?,
        raw_metadata: row.try_get("raw_metadata")?,
        source: row.try_get("source")?,
        ingestion_run_id: row.try_get("ingestion_run_id")?,
        ingested_at: row.try_get("ingested_at")?,
    })
}

fn row_to_target_match(row: &sqlx::postgres::PgRow) -> anyhow::Result<TargetMatch> {
    Ok(TargetMatch {
        id: row.try_get("id")?,
        target_id: row.try_get("target_id")?,
        raw_transaction_id: row.try_get("raw_transaction_id")?,
        match_reason: row.try_get("match_reason")?,
        matched_at: row.try_get("matched_at")?,
    })
}

fn row_to_ingestion_run(row: &sqlx::postgres::PgRow) -> anyhow::Result<IngestionRun> {
    use std::str::FromStr;
    let mode_str: String = row.try_get("mode")?;
    let status_str: String = row.try_get("status")?;
    Ok(IngestionRun {
        id: row.try_get("id")?,
        target_id: row.try_get("target_id")?,
        network: row.try_get("network")?,
        source: row.try_get("source")?,
        mode: IngestionJobMode::from_str(&mode_str)
            .map_err(|e| anyhow::anyhow!("invalid ingestion run mode '{mode_str}': {e}"))?,
        status: IngestionJobStatus::from_str(&status_str)
            .map_err(|e| anyhow::anyhow!("invalid ingestion run status '{status_str}': {e}"))?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        records_written: row.try_get("records_written")?,
        error_message: row.try_get("error_message")?,
        cursor_state: row.try_get("cursor_state")?,
    })
}

fn row_to_checkpoint(row: &sqlx::postgres::PgRow) -> anyhow::Result<Checkpoint> {
    Ok(Checkpoint {
        id: row.try_get("id")?,
        target_id: row.try_get("target_id")?,
        network: row.try_get("network")?,
        source: row.try_get("source")?,
        cursor: row.try_get("cursor")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_dataset_version(row: &sqlx::postgres::PgRow) -> anyhow::Result<DatasetVersion> {
    let status_str: String = row.try_get("status")?;
    Ok(DatasetVersion {
        id: row.try_get("id")?,
        dataset_name: row.try_get("dataset_name")?,
        version: row.try_get("version")?,
        parser_hash: row.try_get("parser_hash")?,
        created_at: row.try_get("created_at")?,
        notes: row.try_get("notes")?,
        status: sql_to_dataset_version_status(&status_str)?,
    })
}

// ---------------------------------------------------------------------------
// Query builders (pub for unit-testing)
// ---------------------------------------------------------------------------

/// Build a batch INSERT for `raw_transactions` with ON CONFLICT DO NOTHING.
pub fn build_raw_transaction_insert(
    txs: &[RawTransaction],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO raw_transactions \
         (id, network, tx_hash, timestamp, block_number, raw_metadata, source, ingestion_run_id, ingested_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, tx) in txs.iter().enumerate() {
        let base = i * 9;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
        ));
        use sqlx::Arguments;
        args.add(tx.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&tx.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&tx.tx_hash).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx.timestamp).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx.block_number)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&tx.raw_metadata)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&tx.source).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx.ingestion_run_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx.ingested_at)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(" ON CONFLICT (network, tx_hash) DO NOTHING");
    Ok((query, args))
}

/// Build a batch INSERT for `raw_transactions` with ON CONFLICT DO UPDATE
/// and RETURNING id, so we always get the canonical row ID back.
pub fn build_raw_transaction_upsert_returning(
    txs: &[RawTransaction],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO raw_transactions \
         (id, network, tx_hash, timestamp, block_number, raw_metadata, source, ingestion_run_id, ingested_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, tx) in txs.iter().enumerate() {
        let base = i * 9;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
        ));
        use sqlx::Arguments;
        args.add(tx.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&tx.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&tx.tx_hash).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx.timestamp).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx.block_number)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&tx.raw_metadata)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&tx.source).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx.ingestion_run_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx.ingested_at)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(" ON CONFLICT (network, tx_hash) DO UPDATE SET updated_at = NOW() RETURNING id");
    Ok((query, args))
}

/// Build a batch INSERT for `target_matches` with ON CONFLICT DO NOTHING.
pub fn build_target_match_insert(
    matches: &[TargetMatch],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO target_matches \
         (id, target_id, raw_transaction_id, match_reason, matched_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, m) in matches.iter().enumerate() {
        let base = i * 5;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
        ));
        use sqlx::Arguments;
        args.add(m.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(m.target_id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(m.raw_transaction_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&m.match_reason)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(m.matched_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(" ON CONFLICT (target_id, raw_transaction_id) DO NOTHING");
    Ok((query, args))
}

/// Build an upsert for a single `index_target`.
///
/// Uses ON CONFLICT on (kind, network, address) to avoid duplicate insert
/// errors when concurrent requests attempt to create the same target.
/// On conflict, only `updated_at` is bumped; the existing row is kept.
pub fn build_index_target_insert(
    t: &IndexTarget,
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let query = String::from(
        "INSERT INTO index_targets \
         (id, kind, network, chain_family, address, filter_spec, mode, label, owner_id, created_at, updated_at) \
         VALUES ($1, $2::target_kind_enum, $3, $4::chain_family_enum, $5, $6, $7::target_mode_enum, $8, $9, $10, $11) \
         ON CONFLICT (kind, network, address, owner_id) WHERE address IS NOT NULL DO UPDATE SET updated_at = NOW() \
         RETURNING id, kind::text, network, chain_family::text, address, filter_spec, mode::text, label, owner_id, created_at, updated_at",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    use sqlx::Arguments;
    args.add(t.id).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(target_kind_to_sql(&t.kind))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&t.network).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(chain_family_to_sql(&t.chain_family))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&t.address).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&t.filter_spec)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(target_mode_to_sql(&t.mode))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&t.label).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(t.owner_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(t.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(t.updated_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((query, args))
}

/// Build the UPSERT for a single V2 `checkpoint`.
pub fn build_checkpoint_upsert(
    cp: &Checkpoint,
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let query = String::from(
        "INSERT INTO checkpoints (id, target_id, network, source, cursor, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (target_id, network, source) \
         DO UPDATE SET cursor = EXCLUDED.cursor, updated_at = EXCLUDED.updated_at",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    use sqlx::Arguments;
    args.add(cp.id).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(cp.target_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&cp.network).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&cp.source).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&cp.cursor).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(cp.updated_at)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((query, args))
}

/// Build the INSERT for a single `ingestion_run`.
pub fn build_ingestion_run_insert(
    run: &IngestionRun,
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let query = String::from(
        "INSERT INTO ingestion_runs \
         (id, target_id, network, source, mode, status, started_at, finished_at, records_written, error_message, cursor_state) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    use sqlx::Arguments;
    args.add(run.id).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(run.target_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&run.network).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&run.source).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(run.mode.to_string())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(run.status.to_string())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(run.started_at)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(run.finished_at)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(run.records_written)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&run.error_message)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&run.cursor_state)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((query, args))
}

/// Build the INSERT for a single `dataset_version`.
pub fn build_dataset_version_insert(
    dv: &DatasetVersion,
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let query = String::from(
        "INSERT INTO dataset_versions (id, dataset_name, version, parser_hash, created_at, notes, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    use sqlx::Arguments;
    args.add(dv.id).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&dv.dataset_name)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dv.version).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&dv.parser_hash)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dv.created_at)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&dv.notes).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dataset_version_status_to_sql(&dv.status))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((query, args))
}

// ---------------------------------------------------------------------------
// Row-mapping helpers for Silver tables (P3-W2)
// ---------------------------------------------------------------------------

fn row_to_token_transfer(row: &sqlx::postgres::PgRow) -> anyhow::Result<TokenTransfer> {
    use bigdecimal::BigDecimal;
    Ok(TokenTransfer {
        id: row.try_get("id")?,
        raw_transaction_id: row.try_get("raw_transaction_id")?,
        network: row.try_get("network")?,
        token_address: row.try_get("token_address")?,
        token_symbol: row.try_get("token_symbol")?,
        from_address: row.try_get("from_address")?,
        to_address: row.try_get("to_address")?,
        amount: row.try_get::<BigDecimal, _>("amount")?,
        decimals: row.try_get("decimals")?,
        transfer_index: row.try_get("transfer_index")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_native_balance_delta(row: &sqlx::postgres::PgRow) -> anyhow::Result<NativeBalanceDelta> {
    use bigdecimal::BigDecimal;
    Ok(NativeBalanceDelta {
        id: row.try_get("id")?,
        raw_transaction_id: row.try_get("raw_transaction_id")?,
        network: row.try_get("network")?,
        account_address: row.try_get("account_address")?,
        native_token: row.try_get("native_token")?,
        pre_balance: row.try_get::<BigDecimal, _>("pre_balance")?,
        post_balance: row.try_get::<BigDecimal, _>("post_balance")?,
        delta: row.try_get::<BigDecimal, _>("delta")?,
        is_fee_payer: row.try_get("is_fee_payer")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

// ---------------------------------------------------------------------------
// Query builders for Silver tables (P3-W2)
// ---------------------------------------------------------------------------

/// Build a batch INSERT for `token_transfers` with ON CONFLICT DO NOTHING.
pub fn build_token_transfer_insert(
    transfers: &[TokenTransfer],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO token_transfers \
         (id, raw_transaction_id, network, token_address, token_symbol, from_address, to_address, amount, decimals, transfer_index, dataset_version_id, created_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, t) in transfers.iter().enumerate() {
        let base = i * 12;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
            base + 10,
            base + 11,
            base + 12,
        ));
        use sqlx::Arguments;
        args.add(t.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.raw_transaction_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.token_address)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.token_symbol)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.from_address)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.to_address)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.amount).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.decimals).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.transfer_index)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.dataset_version_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(
        " ON CONFLICT (raw_transaction_id, from_address, to_address, token_address, transfer_index) \
         WHERE raw_transaction_id IS NOT NULL DO NOTHING",
    );
    Ok((query, args))
}

/// Build a batch INSERT for `native_balance_deltas` with ON CONFLICT DO NOTHING.
pub fn build_native_balance_delta_insert(
    deltas: &[NativeBalanceDelta],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO native_balance_deltas \
         (id, raw_transaction_id, network, account_address, native_token, pre_balance, post_balance, delta, is_fee_payer, dataset_version_id, created_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, d) in deltas.iter().enumerate() {
        let base = i * 11;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
            base + 10,
            base + 11,
        ));
        use sqlx::Arguments;
        args.add(d.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(d.raw_transaction_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&d.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&d.account_address)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&d.native_token)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&d.pre_balance)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&d.post_balance)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&d.delta).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(d.is_fee_payer)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(d.dataset_version_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(d.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(
        " ON CONFLICT (raw_transaction_id, account_address, native_token) \
         WHERE raw_transaction_id IS NOT NULL DO NOTHING",
    );
    Ok((query, args))
}

// ---------------------------------------------------------------------------
// Row-mapping helpers for Silver tables (P3-W3)
// ---------------------------------------------------------------------------

fn row_to_decoded_event(row: &sqlx::postgres::PgRow) -> anyhow::Result<DecodedEvent> {
    Ok(DecodedEvent {
        id: row.try_get("id")?,
        raw_transaction_id: row.try_get("raw_transaction_id")?,
        network: row.try_get("network")?,
        program_or_contract: row.try_get("program_or_contract")?,
        event_signature: row.try_get("event_signature")?,
        event_name: row.try_get("event_name")?,
        log_index: row.try_get("log_index")?,
        decoded_fields: row.try_get("decoded_fields")?,
        raw_fields: row.try_get("raw_fields")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

// ---------------------------------------------------------------------------
// Query builders for Silver tables (P3-W3)
// ---------------------------------------------------------------------------

/// Build a batch INSERT for `decoded_events` with ON CONFLICT DO NOTHING.
pub fn build_decoded_event_insert(
    events: &[DecodedEvent],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO decoded_events \
         (id, raw_transaction_id, network, program_or_contract, event_signature, event_name, log_index, decoded_fields, raw_fields, dataset_version_id, created_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, e) in events.iter().enumerate() {
        let base = i * 11;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
            base + 10,
            base + 11,
        ));
        use sqlx::Arguments;
        args.add(e.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(e.raw_transaction_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&e.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&e.program_or_contract)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&e.event_signature)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&e.event_name)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(e.log_index).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&e.decoded_fields)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&e.raw_fields)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(e.dataset_version_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(e.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(
        " ON CONFLICT (raw_transaction_id, program_or_contract, log_index) \
         WHERE raw_transaction_id IS NOT NULL DO NOTHING",
    );
    Ok((query, args))
}

// ---------------------------------------------------------------------------
// Row-mapping helpers for Silver tables (P3-W4)
// ---------------------------------------------------------------------------

fn row_to_hl_fill_record(row: &sqlx::postgres::PgRow) -> anyhow::Result<HlFillRecord> {
    Ok(HlFillRecord {
        id: row.try_get("id")?,
        raw_transaction_id: row.try_get("raw_transaction_id")?,
        network: row.try_get("network")?,
        coin: row.try_get("coin")?,
        side: row.try_get("side")?,
        price: row.try_get("price")?,
        size: row.try_get("size")?,
        direction: row.try_get("direction")?,
        closed_pnl: row.try_get("closed_pnl")?,
        fee: row.try_get("fee")?,
        fee_token: row.try_get("fee_token")?,
        fill_time: row.try_get("fill_time")?,
        order_id: row.try_get("order_id")?,
        trade_id: row.try_get("trade_id")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_hl_funding_payment(row: &sqlx::postgres::PgRow) -> anyhow::Result<HlFundingPayment> {
    Ok(HlFundingPayment {
        id: row.try_get("id")?,
        raw_transaction_id: row.try_get("raw_transaction_id")?,
        network: row.try_get("network")?,
        coin: row.try_get("coin")?,
        amount: row.try_get("amount")?,
        funding_rate: row.try_get("funding_rate")?,
        payment_time: row.try_get("payment_time")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_hl_position_change(row: &sqlx::postgres::PgRow) -> anyhow::Result<HlPositionChange> {
    Ok(HlPositionChange {
        id: row.try_get("id")?,
        raw_transaction_id: row.try_get("raw_transaction_id")?,
        network: row.try_get("network")?,
        coin: row.try_get("coin")?,
        side: row.try_get("side")?,
        size_delta: row.try_get("size_delta")?,
        price: row.try_get("price")?,
        direction: row.try_get("direction")?,
        source_event: row.try_get("source_event")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

// ---------------------------------------------------------------------------
// Query builders for Silver tables (P3-W4)
// ---------------------------------------------------------------------------

/// Build a batch INSERT for `hl_fill_records` with ON CONFLICT DO NOTHING.
pub fn build_hl_fill_record_insert(
    fills: &[HlFillRecord],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO hl_fill_records \
         (id, raw_transaction_id, network, coin, side, price, size, direction, closed_pnl, fee, fee_token, fill_time, order_id, trade_id, dataset_version_id, created_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, f) in fills.iter().enumerate() {
        let base = i * 16;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
            base + 10,
            base + 11,
            base + 12,
            base + 13,
            base + 14,
            base + 15,
            base + 16,
        ));
        use sqlx::Arguments;
        args.add(f.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(f.raw_transaction_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.coin).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.side).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.price).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.size).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.direction).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.closed_pnl)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.fee).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&f.fee_token).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(f.fill_time).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(f.order_id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(f.trade_id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(f.dataset_version_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(f.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(
        " ON CONFLICT (raw_transaction_id, coin, fill_time, side) \
         WHERE raw_transaction_id IS NOT NULL DO NOTHING",
    );
    Ok((query, args))
}

/// Build a batch INSERT for `hl_funding_payments` with ON CONFLICT DO NOTHING.
pub fn build_hl_funding_payment_insert(
    payments: &[HlFundingPayment],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO hl_funding_payments \
         (id, raw_transaction_id, network, coin, amount, funding_rate, payment_time, dataset_version_id, created_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, p) in payments.iter().enumerate() {
        let base = i * 9;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
        ));
        use sqlx::Arguments;
        args.add(p.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(p.raw_transaction_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&p.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&p.coin).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&p.amount).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&p.funding_rate)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(p.payment_time)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(p.dataset_version_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(p.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(
        " ON CONFLICT (raw_transaction_id, coin, payment_time) \
         WHERE raw_transaction_id IS NOT NULL DO NOTHING",
    );
    Ok((query, args))
}

/// Build a batch INSERT for `hl_position_changes` with ON CONFLICT DO NOTHING.
pub fn build_hl_position_change_insert(
    changes: &[HlPositionChange],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO hl_position_changes \
         (id, raw_transaction_id, network, coin, side, size_delta, price, direction, source_event, dataset_version_id, created_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, c) in changes.iter().enumerate() {
        let base = i * 11;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
            base + 9,
            base + 10,
            base + 11,
        ));
        use sqlx::Arguments;
        args.add(c.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(c.raw_transaction_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&c.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&c.coin).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&c.side).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&c.size_delta)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&c.price).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&c.direction).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&c.source_event)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(c.dataset_version_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(c.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(
        " ON CONFLICT (raw_transaction_id, coin, side, source_event) \
         WHERE raw_transaction_id IS NOT NULL DO NOTHING",
    );
    Ok((query, args))
}

// ---------------------------------------------------------------------------
// Row-mapping helpers for dataset_completeness (P3-W6)
// ---------------------------------------------------------------------------

fn row_to_dataset_completeness(row: &sqlx::postgres::PgRow) -> anyhow::Result<DatasetCompleteness> {
    let status_str: String = row.try_get("status")?;
    Ok(DatasetCompleteness {
        id: row.try_get("id")?,
        target_id: row.try_get("target_id")?,
        dataset_name: row.try_get("dataset_name")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        network: row.try_get("network")?,
        status: sql_to_completeness_status(&status_str)?,
        coverage_start: row.try_get("coverage_start")?,
        coverage_end: row.try_get("coverage_end")?,
        block_start: row.try_get("block_start")?,
        block_end: row.try_get("block_end")?,
        last_ingestion_run_id: row.try_get("last_ingestion_run_id")?,
        records_count: row.try_get("records_count")?,
        gap_ranges: row.try_get("gap_ranges")?,
        notes: row.try_get("notes")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_wallet_ledger_record(row: &sqlx::postgres::PgRow) -> anyhow::Result<WalletLedgerRecord> {
    Ok(WalletLedgerRecord {
        id: row.try_get("id")?,
        raw_transaction_id: row.try_get("raw_transaction_id")?,
        wallet_address: row.try_get("wallet_address")?,
        network: row.try_get("network")?,
        tx_hash: row.try_get("tx_hash")?,
        timestamp: row.try_get("timestamp")?,
        entry_type: row.try_get("entry_type")?,
        asset_symbol: row.try_get("asset_symbol")?,
        amount: row.try_get("amount")?,
        counterparty_address: row.try_get("counterparty_address")?,
        fee_amount: row.try_get("fee_amount")?,
        fee_asset: row.try_get("fee_asset")?,
        cost_basis: row.try_get("cost_basis")?,
        proceeds: row.try_get("proceeds")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_balance_snapshot(row: &sqlx::postgres::PgRow) -> anyhow::Result<BalanceSnapshot> {
    Ok(BalanceSnapshot {
        id: row.try_get("id")?,
        wallet_address: row.try_get("wallet_address")?,
        asset_symbol: row.try_get("asset_symbol")?,
        network: row.try_get("network")?,
        timestamp: row.try_get("timestamp")?,
        balance: row.try_get("balance")?,
        tx_hash: row.try_get("tx_hash")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_hl_pnl_summary(row: &sqlx::postgres::PgRow) -> anyhow::Result<HlPnlSummary> {
    Ok(HlPnlSummary {
        id: row.try_get("id")?,
        wallet_address: row.try_get("wallet_address")?,
        coin: row.try_get("coin")?,
        network: row.try_get("network")?,
        period_start: row.try_get("period_start")?,
        period_end: row.try_get("period_end")?,
        total_closed_pnl: row.try_get("total_closed_pnl")?,
        total_funding: row.try_get("total_funding")?,
        total_fees: row.try_get("total_fees")?,
        net_pnl: row.try_get("net_pnl")?,
        trade_count: row.try_get("trade_count")?,
        fill_count: row.try_get("fill_count")?,
        avg_trade_size: row.try_get("avg_trade_size")?,
        win_count: row.try_get("win_count")?,
        loss_count: row.try_get("loss_count")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_hl_trade_history(row: &sqlx::postgres::PgRow) -> anyhow::Result<HlTradeHistory> {
    Ok(HlTradeHistory {
        id: row.try_get("id")?,
        wallet_address: row.try_get("wallet_address")?,
        coin: row.try_get("coin")?,
        network: row.try_get("network")?,
        side: row.try_get("side")?,
        entry_price: row.try_get("entry_price")?,
        exit_price: row.try_get("exit_price")?,
        size: row.try_get("size")?,
        opened_at: row.try_get("opened_at")?,
        closed_at: row.try_get("closed_at")?,
        realized_pnl: row.try_get("realized_pnl")?,
        fees: row.try_get("fees")?,
        num_fills: row.try_get("num_fills")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_protocol_event(row: &sqlx::postgres::PgRow) -> anyhow::Result<ProtocolEvent> {
    Ok(ProtocolEvent {
        id: row.try_get("id")?,
        network: row.try_get("network")?,
        protocol_address: row.try_get("protocol_address")?,
        protocol_name: row.try_get("protocol_name")?,
        event_type: row.try_get("event_type")?,
        event_details: row.try_get("event_details")?,
        pool_address: row.try_get("pool_address")?,
        raw_event_id: row.try_get("raw_event_id")?,
        timestamp: row.try_get("timestamp")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_pool_snapshot(row: &sqlx::postgres::PgRow) -> anyhow::Result<PoolSnapshot> {
    Ok(PoolSnapshot {
        id: row.try_get("id")?,
        network: row.try_get("network")?,
        pool_address: row.try_get("pool_address")?,
        protocol_address: row.try_get("protocol_address")?,
        protocol_name: row.try_get("protocol_name")?,
        token0_address: row.try_get("token0_address")?,
        token0_symbol: row.try_get("token0_symbol")?,
        token1_address: row.try_get("token1_address")?,
        token1_symbol: row.try_get("token1_symbol")?,
        reserve0: row.try_get("reserve0")?,
        reserve1: row.try_get("reserve1")?,
        tvl_usd: row.try_get("tvl_usd")?,
        snapshot_timestamp: row.try_get("snapshot_timestamp")?,
        block_number: row.try_get("block_number")?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        created_at: row.try_get("created_at")?,
    })
}

// ---------------------------------------------------------------------------
// Query builders for dataset_completeness (P3-W6)
// ---------------------------------------------------------------------------

/// Build an UPSERT for a single `dataset_completeness` record.
/// ON CONFLICT on (target_id, dataset_name, network) updates all mutable fields.
pub fn build_dataset_completeness_upsert(
    dc: &DatasetCompleteness,
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let query = String::from(
        "INSERT INTO dataset_completeness \
         (id, target_id, dataset_name, dataset_version_id, network, status, \
          coverage_start, coverage_end, block_start, block_end, \
          last_ingestion_run_id, records_count, gap_ranges, notes, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
         ON CONFLICT (target_id, dataset_name, network) \
         DO UPDATE SET \
             dataset_version_id = EXCLUDED.dataset_version_id, \
             status = EXCLUDED.status, \
             coverage_start = EXCLUDED.coverage_start, \
             coverage_end = EXCLUDED.coverage_end, \
             block_start = EXCLUDED.block_start, \
             block_end = EXCLUDED.block_end, \
             last_ingestion_run_id = EXCLUDED.last_ingestion_run_id, \
             records_count = EXCLUDED.records_count, \
             gap_ranges = EXCLUDED.gap_ranges, \
             notes = EXCLUDED.notes, \
             updated_at = EXCLUDED.updated_at",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    use sqlx::Arguments;
    args.add(dc.id).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.target_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&dc.dataset_name)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.dataset_version_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&dc.network).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(completeness_status_to_sql(&dc.status))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.coverage_start)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.coverage_end)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.block_start)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.block_end).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.last_ingestion_run_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.records_count)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&dc.gap_ranges)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&dc.notes).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.created_at)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(dc.updated_at)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((query, args))
}

// ---------------------------------------------------------------------------
// Dataset filter query builder (P4-W1)
// ---------------------------------------------------------------------------

// -----------------------------------------------------------------------
// Snapshotted streaming export types (fixes PR #238 [P1] pagination)
// -----------------------------------------------------------------------

/// One page of records produced by [`Repository::stream_export_snapshot`].
///
/// Each variant carries the owned `Vec` for one page, keyed by dataset.
/// The consumer (the API-side export writer) matches the variant, hands
/// the slice to the matching CSV/JSONL row-writer, and writes the result
/// directly to disk.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ExportRecordBatch {
    TokenTransfers(Vec<TokenTransfer>),
    NativeBalanceDeltas(Vec<NativeBalanceDelta>),
    DecodedEvents(Vec<DecodedEvent>),
    HlFills(Vec<HlFillRecord>),
    HlFunding(Vec<HlFundingPayment>),
    Positions(Vec<HlPositionChange>),
    WalletLedger(Vec<WalletLedgerRecord>),
    BalanceHistory(Vec<BalanceSnapshot>),
    HlPnlSummary(Vec<HlPnlSummary>),
    HlTradeHistory(Vec<HlTradeHistory>),
    ProtocolEvents(Vec<ProtocolEvent>),
    PoolSnapshots(Vec<PoolSnapshot>),
}

impl ExportRecordBatch {
    /// Number of records in this page. Used by the consumer to decide
    /// whether a short page (`< page_size`) means the snapshot is
    /// exhausted.
    pub fn len(&self) -> usize {
        match self {
            ExportRecordBatch::TokenTransfers(v) => v.len(),
            ExportRecordBatch::NativeBalanceDeltas(v) => v.len(),
            ExportRecordBatch::DecodedEvents(v) => v.len(),
            ExportRecordBatch::HlFills(v) => v.len(),
            ExportRecordBatch::HlFunding(v) => v.len(),
            ExportRecordBatch::Positions(v) => v.len(),
            ExportRecordBatch::WalletLedger(v) => v.len(),
            ExportRecordBatch::BalanceHistory(v) => v.len(),
            ExportRecordBatch::HlPnlSummary(v) => v.len(),
            ExportRecordBatch::HlTradeHistory(v) => v.len(),
            ExportRecordBatch::ProtocolEvents(v) => v.len(),
            ExportRecordBatch::PoolSnapshots(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Run a paged `SELECT ... LIMIT/OFFSET` inside the given transaction,
/// mapping each page via `row_fn` and forwarding the resulting batch via
/// `batch_fn` to `tx_out`. Honors the `cancel` token both before every
/// query and before every send so lease-loss (heartbeat reporting the
/// export job was reclaimed) aborts promptly.
#[allow(clippy::too_many_arguments)]
async fn stream_paged_in_tx<T, R, B>(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cols: &str,
    table: &str,
    order_col: &str,
    target_id: Option<Uuid>,
    network: Option<&str>,
    time_start: Option<i64>,
    time_end: Option<i64>,
    page_size: i64,
    hard_cap: i64,
    cancel: &tokio_util::sync::CancellationToken,
    tx_out: &tokio::sync::mpsc::Sender<ExportRecordBatch>,
    row_fn: R,
    batch_fn: B,
) -> anyhow::Result<usize>
where
    R: Fn(&sqlx::postgres::PgRow) -> anyhow::Result<T>,
    B: Fn(Vec<T>) -> ExportRecordBatch,
{
    let mut offset: i64 = 0;
    let mut total: usize = 0;
    loop {
        if cancel.is_cancelled() {
            anyhow::bail!("export snapshot cancelled (lease lost or worker shutdown)");
        }

        let (sql, args) = build_dataset_filter_query(
            cols, table, order_col, target_id, network, time_start, time_end, page_size, offset,
        )?;

        let rows = sqlx::query_with(&sql, args).fetch_all(&mut **tx).await?;
        let n = rows.len();
        if n == 0 {
            break;
        }

        let records: Vec<T> = rows
            .iter()
            .map(&row_fn)
            .collect::<anyhow::Result<Vec<_>>>()?;

        // The send itself must be cancellation-aware: the channel has
        // bounded capacity (PAGE_CHANNEL_CAPACITY), so if the disk
        // writer errors or the lease is lost while the channel is
        // full, a plain `.await` on `send` would block forever (the
        // consumer is no longer polling `recv`, and only a receiver
        // drop wakes a blocked send). Racing it against `cancelled()`
        // lets the producer exit promptly and roll back the snapshot
        // transaction.
        let batch = batch_fn(records);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                anyhow::bail!("export snapshot cancelled (lease lost or worker shutdown)");
            }
            res = tx_out.send(batch) => {
                if res.is_err() {
                    // Consumer dropped the receiver — treat as cancellation
                    // so we short-circuit the remaining pages and roll back
                    // the snapshot transaction.
                    anyhow::bail!("export consumer dropped the record-batch channel");
                }
            }
        }

        total += n;
        offset += n as i64;

        // Short page means the snapshot is exhausted for this dataset.
        if (n as i64) < page_size {
            break;
        }
        if offset >= hard_cap {
            tracing::warn!(
                table,
                offset,
                hard_cap,
                "Export hit EXPORT_HARD_CAP — truncating stream"
            );
            break;
        }
    }
    Ok(total)
}

/// Build a filtered SELECT query for a Silver dataset table.
///
/// Dynamically adds JOIN and WHERE clauses based on which filters are
/// provided, avoiding unnecessary JOINs when filters are unused.
/// Uses `raw_transactions.timestamp` for time-window filtering so that
/// time_start/time_end are always in the same units (seconds since epoch).
#[allow(clippy::too_many_arguments)]
pub fn build_dataset_filter_query(
    select_cols: &str,
    table_name: &str,
    order_col: &str,
    target_id: Option<Uuid>,
    network: Option<&str>,
    time_start: Option<i64>,
    time_end: Option<i64>,
    limit: i64,
    offset: i64,
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let limit = limit.min(MAX_QUERY_LIMIT);
    let mut sql = format!("SELECT {select_cols} FROM {table_name} dt");
    let mut args = sqlx::postgres::PgArguments::default();
    let mut n: usize = 0;
    let mut wheres: Vec<String> = Vec::new();

    if target_id.is_some() {
        sql.push_str(" JOIN target_matches tm ON tm.raw_transaction_id = dt.raw_transaction_id");
    }
    if time_start.is_some() || time_end.is_some() {
        sql.push_str(" JOIN raw_transactions rt ON rt.id = dt.raw_transaction_id");
    }

    if let Some(tid) = target_id {
        n += 1;
        wheres.push(format!("tm.target_id = ${n}"));
        use sqlx::Arguments;
        args.add(tid).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if let Some(net) = network {
        n += 1;
        wheres.push(format!("dt.network = ${n}"));
        use sqlx::Arguments;
        args.add(net.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if let Some(start) = time_start {
        n += 1;
        wheres.push(format!("rt.timestamp >= ${n}"));
        use sqlx::Arguments;
        args.add(start).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    if let Some(end) = time_end {
        n += 1;
        wheres.push(format!("rt.timestamp <= ${n}"));
        use sqlx::Arguments;
        args.add(end).map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    if !wheres.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&wheres.join(" AND "));
    }

    n += 1;
    let lim_n = n;
    n += 1;
    let off_n = n;
    // `order_col` alone is not a total order — on ties the physical row order
    // is not stable, so OFFSET pagination can reshuffle rows across pages.
    // Append `dt.id DESC` as a deterministic tiebreaker so pages are
    // disjoint and cover the full result set exactly once, both for
    // one-shot queries and for the snapshotted paged export in
    // [`Repository::stream_export_snapshot`]. This fixes the PR #238
    // [P1] correctness concern about LIMIT/OFFSET exports losing or
    // duplicating rows across page boundaries (#208).
    sql.push_str(&format!(
        " ORDER BY {order_col} DESC, dt.id DESC LIMIT ${lim_n} OFFSET ${off_n}"
    ));
    use sqlx::Arguments;
    args.add(limit).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(offset).map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok((sql, args))
}

/// Build a batch INSERT for `raw_evm_traces` with ON CONFLICT DO NOTHING.
pub fn build_raw_evm_trace_insert(
    traces: &[RawEvmTrace],
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let mut query = String::from(
        "INSERT INTO raw_evm_traces \
         (id, transaction_hash, block_number, network, trace_type, raw_trace, ingestion_run_id, created_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, t) in traces.iter().enumerate() {
        let base = i * 8;
        if i > 0 {
            query.push_str(", ");
        }
        query.push_str(&format!(
            "(${}, ${}, ${}, ${}, ${}, ${}, ${}, ${})",
            base + 1,
            base + 2,
            base + 3,
            base + 4,
            base + 5,
            base + 6,
            base + 7,
            base + 8,
        ));
        use sqlx::Arguments;
        args.add(t.id).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.transaction_hash)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.block_number)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.network).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.trace_type.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(&t.raw_trace).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.ingestion_run_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(" ON CONFLICT (network, transaction_hash, trace_type) DO NOTHING");
    Ok((query, args))
}

// ---------------------------------------------------------------------------
// Row-mapping helpers for Durable Control Plane tables
// ---------------------------------------------------------------------------

fn row_to_ingestion_job(row: &sqlx::postgres::PgRow) -> anyhow::Result<IngestionJob> {
    use std::str::FromStr;
    let status_str: String = row.try_get("status")?;
    let mode_str: String = row.try_get("mode")?;
    Ok(IngestionJob {
        id: row.try_get("id")?,
        target_id: row.try_get("target_id")?,
        network: row.try_get("network")?,
        mode: IngestionJobMode::from_str(&mode_str)
            .map_err(|e| anyhow::anyhow!("invalid ingestion_job mode '{mode_str}': {e}"))?,
        status: IngestionJobStatus::from_str(&status_str)
            .map_err(|e| anyhow::anyhow!("invalid ingestion_job status '{status_str}': {e}"))?,
        priority: row.try_get("priority")?,
        idempotency_key: row.try_get("idempotency_key")?,
        requested_by: row.try_get("requested_by")?,
        callback_url: row.try_get("callback_url")?,
        error_message: row.try_get("error_message")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_stream_subscription(row: &sqlx::postgres::PgRow) -> anyhow::Result<StreamSubscription> {
    use std::str::FromStr;
    let source_str: String = row.try_get("source")?;
    let desired_str: String = row.try_get("desired_status")?;
    let actual_str: String = row.try_get("actual_status")?;
    Ok(StreamSubscription {
        id: row.try_get("id")?,
        target_id: row.try_get("target_id")?,
        network: row.try_get("network")?,
        source: StreamSource::from_str(&source_str)
            .map_err(|e| anyhow::anyhow!("invalid stream source '{source_str}': {e}"))?,
        desired_status: StreamDesiredStatus::from_str(&desired_str)
            .map_err(|e| anyhow::anyhow!("invalid desired_status '{desired_str}': {e}"))?,
        actual_status: StreamActualStatus::from_str(&actual_str)
            .map_err(|e| anyhow::anyhow!("invalid actual_status '{actual_str}': {e}"))?,
        lease_owner: row.try_get("lease_owner")?,
        heartbeat_at: row.try_get("heartbeat_at")?,
        cursor_state: row.try_get("cursor_state")?,
        config: row.try_get("config")?,
        error_message: row.try_get("error_message")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn row_to_export_job(row: &sqlx::postgres::PgRow) -> anyhow::Result<ExportJob> {
    use std::str::FromStr;
    let status_str: String = row.try_get("status")?;
    let dataset_str: String = row.try_get("dataset")?;
    let format_str: String = row.try_get("format")?;
    Ok(ExportJob {
        id: row.try_get("id")?,
        dataset: DatasetName::from_str(&dataset_str)
            .map_err(|e| anyhow::anyhow!("invalid export_job dataset '{dataset_str}': {e}"))?,
        format: ExportFormat::from_str(&format_str)
            .map_err(|e| anyhow::anyhow!("invalid export_job format '{format_str}': {e}"))?,
        filters: row.try_get("filters")?,
        sink_config: row.try_get("sink_config")?,
        status: ExportJobStatus::from_str(&status_str)
            .map_err(|e| anyhow::anyhow!("invalid export_job status '{status_str}': {e}"))?,
        worker_id: row.try_get("worker_id")?,
        record_count: row.try_get("record_count")?,
        result_location: row.try_get("result_location")?,
        delivery_destination: row.try_get("delivery_destination").unwrap_or(None),
        error_message: row.try_get("error_message")?,
        dataset_version_id: row.try_get("dataset_version_id").unwrap_or(None),
        dataset_version: row.try_get("dataset_version").unwrap_or(None),
        completeness_status: row.try_get("completeness_status").unwrap_or(None),
        completeness_coverage: row.try_get("completeness_coverage").unwrap_or(None),
        last_ingestion_run_id: row.try_get("last_ingestion_run_id").unwrap_or(None),
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        started_at: row.try_get("started_at")?,
        completed_at: row.try_get("completed_at")?,
        heartbeat_at: row.try_get("heartbeat_at")?,
        owner_id: row.try_get("owner_id").unwrap_or(None),
    })
}
fn row_to_api_key(row: &sqlx::postgres::PgRow) -> anyhow::Result<ApiKey> {
    Ok(ApiKey {
        id: row.try_get("id")?,
        key_hash: row.try_get("key_hash")?,
        name: row.try_get("name")?,
        owner_id: row.try_get("owner_id")?,
        created_at: row.try_get("created_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

fn row_to_materialization_run(row: &sqlx::postgres::PgRow) -> anyhow::Result<MaterializationRun> {
    use std::str::FromStr;
    let status_str: String = row.try_get("status")?;
    Ok(MaterializationRun {
        id: row.try_get("id")?,
        dataset_name: row.try_get("dataset_name")?,
        scope: row.try_get("scope")?,
        input_watermark: row.try_get("input_watermark")?,
        output_record_count: row.try_get("output_record_count")?,
        status: MaterializationRunStatus::from_str(&status_str).map_err(|e| {
            anyhow::anyhow!("invalid materialization_run status '{status_str}': {e}")
        })?,
        dataset_version_id: row.try_get("dataset_version_id")?,
        worker_id: row.try_get("worker_id")?,
        started_at: row.try_get("started_at")?,
        finished_at: row.try_get("finished_at")?,
        heartbeat_at: row.try_get("heartbeat_at")?,
        error_message: row.try_get("error_message")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

/// Map a database row to a `RawEvmTrace`.
fn row_to_raw_evm_trace(row: &sqlx::postgres::PgRow) -> anyhow::Result<RawEvmTrace> {
    use std::str::FromStr;
    Ok(RawEvmTrace {
        id: row.try_get("id")?,
        transaction_hash: row.try_get("transaction_hash")?,
        block_number: row.try_get("block_number")?,
        network: row.try_get("network")?,
        trace_type: EvmTraceType::from_str(row.try_get::<String, _>("trace_type")?.as_str())
            .map_err(|e| anyhow::anyhow!("invalid trace_type: {e}"))?,
        raw_trace: row.try_get("raw_trace")?,
        ingestion_run_id: row.try_get("ingestion_run_id")?,
        created_at: row.try_get("created_at")?,
    })
}

// ---------------------------------------------------------------------------
// Durable Control Plane parameter structs
// ---------------------------------------------------------------------------

/// Parameters for enqueueing an ingestion job.
pub struct EnqueueIngestionJobParams<'a> {
    pub target_id: Option<Uuid>,
    pub network: &'a str,
    pub mode: &'a str,
    pub priority: i32,
    pub idempotency_key: Option<&'a str>,
    pub requested_by: Option<&'a str>,
    pub callback_url: Option<&'a str>,
}

// ---------------------------------------------------------------------------
// V2 Repository impl
// ---------------------------------------------------------------------------

impl Repository {
    /// Batch size for V2 chunked inserts.
    const V2_BATCH_SIZE: usize = 500;

    /// Threshold after which a claimed/running job with no heartbeat is
    /// considered abandoned by a dead worker and eligible for reclaim.
    const STALE_JOB_THRESHOLD_MINUTES: i64 = 5;

    // -----------------------------------------------------------------------
    // Networks
    // -----------------------------------------------------------------------

    pub async fn get_network(&self, id: &str) -> anyhow::Result<Option<Network>> {
        let row = sqlx::query(
            "SELECT id, chain_family::text, display_name, is_testnet, finality_model, block_time_ms \
             FROM networks WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_network).transpose()
    }

    pub async fn list_networks(&self) -> anyhow::Result<Vec<Network>> {
        let rows = sqlx::query(
            "SELECT id, chain_family::text, display_name, is_testnet, finality_model, block_time_ms \
             FROM networks ORDER BY id",
        )
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_network).collect()
    }

    pub async fn list_networks_by_family(
        &self,
        family: ChainFamily,
    ) -> anyhow::Result<Vec<Network>> {
        let rows = sqlx::query(
            "SELECT id, chain_family::text, display_name, is_testnet, finality_model, block_time_ms \
             FROM networks WHERE chain_family = $1::chain_family_enum ORDER BY id",
        )
        .bind(chain_family_to_sql(&family))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_network).collect()
    }

    // -----------------------------------------------------------------------
    // IndexTargets
    // -----------------------------------------------------------------------

    pub async fn create_index_target(&self, target: &IndexTarget) -> anyhow::Result<IndexTarget> {
        let (query, args) = build_index_target_insert(target)?;
        sqlx::query_with(&query, args).execute(self.pool()).await?;
        Ok(target.clone())
    }

    pub async fn get_index_target(&self, id: Uuid) -> anyhow::Result<Option<IndexTarget>> {
        let row = sqlx::query(
            "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
             mode::text, label, owner_id, created_at, updated_at \
             FROM index_targets WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_index_target).transpose()
    }

    pub async fn get_index_target_by_address(
        &self,
        kind: TargetKind,
        network: &str,
        address: &str,
        owner_id: Option<Uuid>,
    ) -> anyhow::Result<Option<IndexTarget>> {
        let row = if let Some(oid) = owner_id {
            sqlx::query(
                "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
                 mode::text, label, owner_id, created_at, updated_at \
                 FROM index_targets \
                 WHERE kind = $1::target_kind_enum AND network = $2 AND address = $3 AND owner_id = $4",
            )
            .bind(target_kind_to_sql(&kind))
            .bind(network)
            .bind(address)
            .bind(oid)
            .fetch_optional(self.pool())
            .await?
        } else {
            sqlx::query(
                "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
                 mode::text, label, owner_id, created_at, updated_at \
                 FROM index_targets \
                 WHERE kind = $1::target_kind_enum AND network = $2 AND address = $3 AND owner_id IS NULL",
            )
            .bind(target_kind_to_sql(&kind))
            .bind(network)
            .bind(address)
            .fetch_optional(self.pool())
            .await?
        };
        row.as_ref().map(row_to_index_target).transpose()
    }

    pub async fn list_index_targets_by_network(
        &self,
        network: &str,
    ) -> anyhow::Result<Vec<IndexTarget>> {
        let rows = sqlx::query(
            "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
             mode::text, label, owner_id, created_at, updated_at \
             FROM index_targets WHERE network = $1 ORDER BY created_at",
        )
        .bind(network)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_index_target).collect()
    }

    pub async fn list_index_targets(
        &self,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<IndexTarget>> {
        let rows = sqlx::query(
            "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
             mode::text, label, owner_id, created_at, updated_at \
             FROM index_targets ORDER BY created_at LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_index_target).collect()
    }

    pub async fn list_index_targets_by_kind(
        &self,
        kind: TargetKind,
    ) -> anyhow::Result<Vec<IndexTarget>> {
        let rows = sqlx::query(
            "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
             mode::text, label, owner_id, created_at, updated_at \
             FROM index_targets WHERE kind = $1::target_kind_enum ORDER BY created_at",
        )
        .bind(target_kind_to_sql(&kind))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_index_target).collect()
    }

    /// Unified target listing with optional filters and pagination.
    /// Both `network` and `kind` can be applied simultaneously.
    pub async fn list_index_targets_filtered(
        &self,
        network: Option<&str>,
        kind: Option<TargetKind>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<IndexTarget>> {
        // Build query dynamically based on which filters are present.
        // Parameter positions are always: $1 = limit, $2 = offset, then
        // optional filter params starting at $3.
        let mut sql = String::from(
            "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
             mode::text, label, owner_id, created_at, updated_at \
             FROM index_targets",
        );

        let mut conditions: Vec<String> = Vec::new();
        let mut param_idx = 3_usize; // $1 and $2 are limit and offset

        if network.is_some() {
            conditions.push(format!("network = ${param_idx}"));
            param_idx += 1;
        }
        if kind.is_some() {
            conditions.push(format!("kind = ${param_idx}::target_kind_enum"));
        }

        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        sql.push_str(" ORDER BY created_at LIMIT $1 OFFSET $2");

        let mut query = sqlx::query(&sql).bind(limit).bind(offset);
        if let Some(n) = network {
            query = query.bind(n.to_string());
        }
        if let Some(ref k) = kind {
            query = query.bind(target_kind_to_sql(k).to_string());
        }

        let rows = query.fetch_all(self.pool()).await?;
        rows.iter().map(row_to_index_target).collect()
    }

    /// List all index targets for a given owner, ordered by created_at.
    /// Used for tenant-scoped target listing where pagination must be applied
    /// after filtering (not before).
    pub async fn list_index_targets_by_owner(
        &self,
        owner_id: Uuid,
        network: Option<&str>,
        kind: Option<TargetKind>,
    ) -> anyhow::Result<Vec<IndexTarget>> {
        let mut sql = String::from(
            "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
             mode::text, label, owner_id, created_at, updated_at \
             FROM index_targets WHERE owner_id = $1",
        );
        let mut param_idx = 2_usize;
        if network.is_some() {
            sql.push_str(&format!(" AND network = ${param_idx}"));
            param_idx += 1;
        }
        if kind.is_some() {
            sql.push_str(&format!(" AND kind = ${param_idx}::target_kind_enum"));
        }
        sql.push_str(" ORDER BY created_at");

        let mut query = sqlx::query(&sql).bind(owner_id);
        if let Some(nw) = network {
            query = query.bind(nw.to_string());
        }
        if let Some(ref k) = kind {
            query = query.bind(target_kind_to_sql(k).to_string());
        }
        let rows = query.fetch_all(self.pool()).await?;
        rows.iter().map(row_to_index_target).collect()
    }

    // -----------------------------------------------------------------------
    // RawTransactions
    // -----------------------------------------------------------------------

    pub async fn save_raw_transactions(&self, txs: &[RawTransaction]) -> anyhow::Result<()> {
        for chunk in txs.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_raw_transaction_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Insert raw transactions and return the canonical database IDs.
    ///
    /// Uses `ON CONFLICT (network, tx_hash) DO UPDATE SET updated_at = NOW()`
    /// so that the RETURNING clause always yields the canonical row ID, even
    /// when the raw transaction already exists from a different target's
    /// ingestion. This is critical for multi-target deduplication: the same
    /// on-chain transaction is stored once, but multiple targets can reference
    /// it via `target_matches`.
    pub async fn upsert_raw_transactions_returning_ids(
        &self,
        txs: &[RawTransaction],
    ) -> anyhow::Result<Vec<Uuid>> {
        let mut all_ids = Vec::with_capacity(txs.len());
        for chunk in txs.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_raw_transaction_upsert_returning(chunk)?;
            let rows = sqlx::query_with(&query, args)
                .fetch_all(self.pool())
                .await?;
            for row in &rows {
                let id: Uuid = row.try_get("id")?;
                all_ids.push(id);
            }
        }
        Ok(all_ids)
    }

    pub async fn get_raw_transaction_by_hash(
        &self,
        network: &str,
        tx_hash: &str,
    ) -> anyhow::Result<Option<RawTransaction>> {
        let row = sqlx::query(
            "SELECT id, network, tx_hash, timestamp, block_number, raw_metadata, \
             source, ingestion_run_id, ingested_at \
             FROM raw_transactions WHERE network = $1 AND tx_hash = $2",
        )
        .bind(network)
        .bind(tx_hash)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_raw_transaction).transpose()
    }

    pub async fn get_raw_transactions_by_run(
        &self,
        run_id: Uuid,
    ) -> anyhow::Result<Vec<RawTransaction>> {
        let rows = sqlx::query(
            "SELECT id, network, tx_hash, timestamp, block_number, raw_metadata, \
             source, ingestion_run_id, ingested_at \
             FROM raw_transactions WHERE ingestion_run_id = $1 ORDER BY timestamp",
        )
        .bind(run_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_raw_transaction).collect()
    }

    /// Batch-lookup canonical `raw_transactions.id` values by `(network, tx_hash)` pairs.
    ///
    /// Returns a map from `(network, tx_hash)` to the canonical UUID. Pairs that
    /// have no matching row in `raw_transactions` are silently omitted from the
    /// result. Processes in chunks of `V2_BATCH_SIZE` to stay within parameter limits.
    pub async fn lookup_raw_transaction_ids(
        &self,
        pairs: &[(String, String)],
    ) -> anyhow::Result<std::collections::HashMap<(String, String), Uuid>> {
        use sqlx::Row;
        let mut result = std::collections::HashMap::with_capacity(pairs.len());
        for chunk in pairs.chunks(Self::V2_BATCH_SIZE) {
            let networks: Vec<&str> = chunk.iter().map(|(n, _)| n.as_str()).collect();
            let hashes: Vec<&str> = chunk.iter().map(|(_, h)| h.as_str()).collect();
            let rows = sqlx::query(
                "SELECT id, network, tx_hash \
                 FROM raw_transactions \
                 WHERE (network, tx_hash) IN \
                   (SELECT unnest($1::text[]), unnest($2::text[]))",
            )
            .bind(&networks)
            .bind(&hashes)
            .fetch_all(self.pool())
            .await?;
            for row in &rows {
                let id: Uuid = row.try_get("id")?;
                let network: String = row.try_get("network")?;
                let tx_hash: String = row.try_get("tx_hash")?;
                result.insert((network, tx_hash), id);
            }
        }
        Ok(result)
    }

    /// Batch-lookup the actual `network` stored in `raw_transactions` for a set of
    /// `tx_hash` values, without requiring the caller to already know the network.
    ///
    /// Returns a map from `tx_hash` to a `Vec` of `(id, network)` pairs.  The same
    /// `tx_hash` can legitimately exist on multiple networks (cross-chain replays,
    /// L2 re-orgs, etc.), so callers MUST disambiguate using chain family or other
    /// context rather than assuming a single match.
    ///
    /// This is the key method for the normalize path: V1 `Transaction` rows only
    /// carry `chain: Chain`, not `network`, so we must consult Bronze to recover
    /// the actual network before materializing Silver data.
    pub async fn lookup_raw_tx_networks_by_hashes(
        &self,
        tx_hashes: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, Vec<(Uuid, String)>>> {
        use sqlx::Row;
        let mut result: std::collections::HashMap<String, Vec<(Uuid, String)>> =
            std::collections::HashMap::with_capacity(tx_hashes.len());
        for chunk in tx_hashes.chunks(Self::V2_BATCH_SIZE) {
            let hashes: Vec<&str> = chunk.iter().map(|h| h.as_str()).collect();
            let rows = sqlx::query(
                "SELECT id, network, tx_hash \
                 FROM raw_transactions \
                 WHERE tx_hash = ANY($1::text[])",
            )
            .bind(&hashes)
            .fetch_all(self.pool())
            .await?;
            for row in &rows {
                let id: Uuid = row.try_get("id")?;
                let network: String = row.try_get("network")?;
                let tx_hash: String = row.try_get("tx_hash")?;
                result.entry(tx_hash).or_default().push((id, network));
            }
        }
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // TargetMatches
    // -----------------------------------------------------------------------

    pub async fn save_target_matches(&self, matches: &[TargetMatch]) -> anyhow::Result<()> {
        for chunk in matches.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_target_match_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    pub async fn get_matches_by_target(
        &self,
        target_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<(TargetMatch, RawTransaction)>> {
        let rows = sqlx::query(
            "SELECT \
                 tm.id, tm.target_id, tm.raw_transaction_id, tm.match_reason, tm.matched_at, \
                 rt.id AS rt_id, rt.network, rt.tx_hash, rt.timestamp, rt.block_number, \
                 rt.raw_metadata, rt.source, rt.ingestion_run_id, rt.ingested_at \
             FROM target_matches tm \
             JOIN raw_transactions rt ON rt.id = tm.raw_transaction_id \
             WHERE tm.target_id = $1 \
             ORDER BY rt.timestamp DESC \
             LIMIT $2 OFFSET $3",
        )
        .bind(target_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;

        let mut results = Vec::with_capacity(rows.len());
        for row in &rows {
            let tm = TargetMatch {
                id: row.try_get("id")?,
                target_id: row.try_get("target_id")?,
                raw_transaction_id: row.try_get("raw_transaction_id")?,
                match_reason: row.try_get("match_reason")?,
                matched_at: row.try_get("matched_at")?,
            };
            let rt = RawTransaction {
                id: row.try_get("rt_id")?,
                network: row.try_get("network")?,
                tx_hash: row.try_get("tx_hash")?,
                timestamp: row.try_get("timestamp")?,
                block_number: row.try_get("block_number")?,
                raw_metadata: row.try_get("raw_metadata")?,
                source: row.try_get("source")?,
                ingestion_run_id: row.try_get("ingestion_run_id")?,
                ingested_at: row.try_get("ingested_at")?,
            };
            results.push((tm, rt));
        }
        Ok(results)
    }

    pub async fn get_matches_by_raw_tx(&self, raw_tx_id: Uuid) -> anyhow::Result<Vec<TargetMatch>> {
        let rows = sqlx::query(
            "SELECT id, target_id, raw_transaction_id, match_reason, matched_at \
             FROM target_matches WHERE raw_transaction_id = $1 ORDER BY matched_at",
        )
        .bind(raw_tx_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_target_match).collect()
    }

    // -----------------------------------------------------------------------
    // IngestionRuns
    // -----------------------------------------------------------------------

    pub async fn create_ingestion_run(&self, run: &IngestionRun) -> anyhow::Result<()> {
        let (query, args) = build_ingestion_run_insert(run)?;
        sqlx::query_with(&query, args).execute(self.pool()).await?;
        Ok(())
    }

    pub async fn update_ingestion_run_status(
        &self,
        id: Uuid,
        status: &str,
        finished_at: Option<DateTime<Utc>>,
        records_written: i64,
        error_message: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE ingestion_runs SET status = $2, finished_at = $3, \
             records_written = $4, error_message = $5 WHERE id = $1",
        )
        .bind(id)
        .bind(status)
        .bind(finished_at)
        .bind(records_written)
        .bind(error_message)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_ingestion_run(&self, id: Uuid) -> anyhow::Result<Option<IngestionRun>> {
        let row = sqlx::query(
            "SELECT id, target_id, network, source, mode, status, started_at, \
             finished_at, records_written, error_message, cursor_state \
             FROM ingestion_runs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_ingestion_run).transpose()
    }

    pub async fn list_ingestion_runs_by_target(
        &self,
        target_id: Uuid,
    ) -> anyhow::Result<Vec<IngestionRun>> {
        let rows = sqlx::query(
            "SELECT id, target_id, network, source, mode, status, started_at, \
             finished_at, records_written, error_message, cursor_state \
             FROM ingestion_runs WHERE target_id = $1 ORDER BY started_at DESC",
        )
        .bind(target_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_ingestion_run).collect()
    }

    // -----------------------------------------------------------------------
    // Checkpoints (V2)
    // -----------------------------------------------------------------------

    pub async fn upsert_checkpoint_v2(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        let (query, args) = build_checkpoint_upsert(cp)?;
        sqlx::query_with(&query, args).execute(self.pool()).await?;
        Ok(())
    }

    pub async fn get_checkpoint_v2(
        &self,
        target_id: Uuid,
        network: &str,
        source: &str,
    ) -> anyhow::Result<Option<Checkpoint>> {
        let row = sqlx::query(
            "SELECT id, target_id, network, source, cursor, updated_at \
             FROM checkpoints WHERE target_id = $1 AND network = $2 AND source = $3",
        )
        .bind(target_id)
        .bind(network)
        .bind(source)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_checkpoint).transpose()
    }

    // -----------------------------------------------------------------------
    // DatasetVersions
    // -----------------------------------------------------------------------

    pub async fn create_dataset_version(&self, dv: &DatasetVersion) -> anyhow::Result<()> {
        let (query, args) = build_dataset_version_insert(dv)?;
        sqlx::query_with(&query, args).execute(self.pool()).await?;
        Ok(())
    }

    pub async fn get_latest_dataset_version(
        &self,
        dataset_name: &str,
    ) -> anyhow::Result<Option<DatasetVersion>> {
        let row = sqlx::query(
            "SELECT id, dataset_name, version, parser_hash, created_at, notes, status \
             FROM dataset_versions WHERE dataset_name = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(dataset_name)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_dataset_version).transpose()
    }

    /// Get the active dataset version for a given dataset name.
    /// Returns the latest version with status = Active, or None if no active version exists.
    pub async fn get_active_dataset_version(
        &self,
        dataset_name: &str,
    ) -> anyhow::Result<Option<DatasetVersion>> {
        let row = sqlx::query(
            "SELECT id, dataset_name, version, parser_hash, created_at, notes, status \
             FROM dataset_versions \
             WHERE dataset_name = $1 AND status = $2 \
             ORDER BY version DESC LIMIT 1",
        )
        .bind(dataset_name)
        .bind(dataset_version_status_to_sql(&DatasetVersionStatus::Active))
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_dataset_version).transpose()
    }

    // -----------------------------------------------------------------------
    // Dataset lifecycle methods (P3-W1)
    // -----------------------------------------------------------------------

    /// List distinct dataset names that have at least one version.
    pub async fn list_datasets(&self) -> anyhow::Result<Vec<String>> {
        let rows =
            sqlx::query("SELECT DISTINCT dataset_name FROM dataset_versions ORDER BY dataset_name")
                .fetch_all(self.pool())
                .await?;
        let mut names = Vec::with_capacity(rows.len());
        for row in &rows {
            names.push(row.try_get("dataset_name")?);
        }
        Ok(names)
    }

    /// List all versions of a given dataset, ordered by version descending.
    pub async fn list_dataset_versions(
        &self,
        dataset_name: &str,
    ) -> anyhow::Result<Vec<DatasetVersion>> {
        let rows = sqlx::query(
            "SELECT id, dataset_name, version, parser_hash, created_at, notes, status \
             FROM dataset_versions WHERE dataset_name = $1 ORDER BY version DESC",
        )
        .bind(dataset_name)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_dataset_version).collect()
    }

    /// Get a specific dataset version by its ID.
    pub async fn get_dataset_version_by_id(
        &self,
        id: Uuid,
    ) -> anyhow::Result<Option<DatasetVersion>> {
        let row = sqlx::query(
            "SELECT id, dataset_name, version, parser_hash, created_at, notes, status \
             FROM dataset_versions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_dataset_version).transpose()
    }

    /// Mark a dataset version as superseded.
    pub async fn mark_version_superseded(&self, id: Uuid) -> anyhow::Result<()> {
        sqlx::query("UPDATE dataset_versions SET status = $2 WHERE id = $1")
            .bind(id)
            .bind(dataset_version_status_to_sql(
                &DatasetVersionStatus::Superseded,
            ))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Count ledger_entries rows linked to a specific dataset version.
    pub async fn count_records_by_version(&self, dataset_version_id: Uuid) -> anyhow::Result<i64> {
        let row =
            sqlx::query("SELECT COUNT(*) AS cnt FROM ledger_entries WHERE dataset_version_id = $1")
                .bind(dataset_version_id)
                .fetch_one(self.pool())
                .await?;
        let count: i64 = row.try_get("cnt")?;
        Ok(count)
    }

    // -----------------------------------------------------------------------
    // TokenTransfers (P3-W2)
    // -----------------------------------------------------------------------

    /// Bulk insert token transfer records.
    pub async fn save_token_transfers(&self, transfers: &[TokenTransfer]) -> anyhow::Result<()> {
        for chunk in transfers.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_token_transfer_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Query token transfers by address (from or to).
    pub async fn get_token_transfers_by_address(
        &self,
        address: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<TokenTransfer>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, token_address, token_symbol, \
             from_address, to_address, amount, decimals, transfer_index, dataset_version_id, created_at \
             FROM token_transfers \
             WHERE from_address = $1 OR to_address = $1 \
             ORDER BY created_at DESC LIMIT $2 OFFSET $3",
        )
        .bind(address)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_token_transfer).collect()
    }

    /// Query token transfers by raw transaction ID.
    pub async fn get_token_transfers_by_raw_tx(
        &self,
        raw_tx_id: Uuid,
    ) -> anyhow::Result<Vec<TokenTransfer>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, token_address, token_symbol, \
             from_address, to_address, amount, decimals, transfer_index, dataset_version_id, created_at \
             FROM token_transfers WHERE raw_transaction_id = $1 ORDER BY created_at",
        )
        .bind(raw_tx_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_token_transfer).collect()
    }

    // -----------------------------------------------------------------------
    // NativeBalanceDeltas (P3-W2)
    // -----------------------------------------------------------------------

    /// Bulk insert native balance delta records.
    pub async fn save_native_balance_deltas(
        &self,
        deltas: &[NativeBalanceDelta],
    ) -> anyhow::Result<()> {
        for chunk in deltas.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_native_balance_delta_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Query native balance deltas by account address.
    pub async fn get_native_balance_deltas_by_account(
        &self,
        account_address: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<NativeBalanceDelta>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, account_address, native_token, \
             pre_balance, post_balance, delta, is_fee_payer, dataset_version_id, created_at \
             FROM native_balance_deltas \
             WHERE account_address = $1 \
             ORDER BY created_at DESC LIMIT $2 OFFSET $3",
        )
        .bind(account_address)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_native_balance_delta).collect()
    }

    /// Query native balance deltas by raw transaction ID.
    pub async fn get_native_balance_deltas_by_raw_tx(
        &self,
        raw_tx_id: Uuid,
    ) -> anyhow::Result<Vec<NativeBalanceDelta>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, account_address, native_token, \
             pre_balance, post_balance, delta, is_fee_payer, dataset_version_id, created_at \
             FROM native_balance_deltas WHERE raw_transaction_id = $1 ORDER BY created_at",
        )
        .bind(raw_tx_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_native_balance_delta).collect()
    }

    // -----------------------------------------------------------------------
    // DecodedEvents (P3-W3)
    // -----------------------------------------------------------------------

    /// Bulk insert decoded event records.
    pub async fn save_decoded_events(&self, events: &[DecodedEvent]) -> anyhow::Result<()> {
        for chunk in events.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_decoded_event_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Query decoded events by contract/program address.
    pub async fn get_decoded_events_by_contract(
        &self,
        program_or_contract: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<DecodedEvent>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, program_or_contract, event_signature, \
             event_name, log_index, decoded_fields, raw_fields, dataset_version_id, created_at \
             FROM decoded_events \
             WHERE program_or_contract = $1 \
             ORDER BY created_at DESC LIMIT $2 OFFSET $3",
        )
        .bind(program_or_contract)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_decoded_event).collect()
    }

    /// Query decoded events by raw transaction ID.
    pub async fn get_decoded_events_by_raw_tx(
        &self,
        raw_tx_id: Uuid,
    ) -> anyhow::Result<Vec<DecodedEvent>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, program_or_contract, event_signature, \
             event_name, log_index, decoded_fields, raw_fields, dataset_version_id, created_at \
             FROM decoded_events WHERE raw_transaction_id = $1 ORDER BY log_index",
        )
        .bind(raw_tx_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_decoded_event).collect()
    }

    // -----------------------------------------------------------------------
    // HlFillRecords (P3-W4)
    // -----------------------------------------------------------------------

    /// Bulk insert Hyperliquid fill records.
    pub async fn save_hl_fill_records(&self, fills: &[HlFillRecord]) -> anyhow::Result<()> {
        for chunk in fills.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_hl_fill_record_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Query Hyperliquid fill records by coin.
    pub async fn get_hl_fill_records_by_coin(
        &self,
        coin: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<HlFillRecord>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, coin, side, price, size, direction, \
             closed_pnl, fee, fee_token, fill_time, order_id, trade_id, dataset_version_id, created_at \
             FROM hl_fill_records \
             WHERE coin = $1 \
             ORDER BY fill_time DESC LIMIT $2 OFFSET $3",
        )
        .bind(coin)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_hl_fill_record).collect()
    }

    /// Query Hyperliquid fill records by raw transaction ID.
    pub async fn get_hl_fill_records_by_raw_tx(
        &self,
        raw_tx_id: Uuid,
    ) -> anyhow::Result<Vec<HlFillRecord>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, coin, side, price, size, direction, \
             closed_pnl, fee, fee_token, fill_time, order_id, trade_id, dataset_version_id, created_at \
             FROM hl_fill_records WHERE raw_transaction_id = $1 ORDER BY fill_time",
        )
        .bind(raw_tx_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_hl_fill_record).collect()
    }

    // -----------------------------------------------------------------------
    // HlFundingPayments (P3-W4)
    // -----------------------------------------------------------------------

    /// Bulk insert Hyperliquid funding payment records.
    pub async fn save_hl_funding_payments(
        &self,
        payments: &[HlFundingPayment],
    ) -> anyhow::Result<()> {
        for chunk in payments.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_hl_funding_payment_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Query Hyperliquid funding payments by coin.
    pub async fn get_hl_funding_payments_by_coin(
        &self,
        coin: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<HlFundingPayment>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, coin, amount, funding_rate, \
             payment_time, dataset_version_id, created_at \
             FROM hl_funding_payments \
             WHERE coin = $1 \
             ORDER BY payment_time DESC LIMIT $2 OFFSET $3",
        )
        .bind(coin)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_hl_funding_payment).collect()
    }

    /// Query Hyperliquid funding payments by raw transaction ID.
    pub async fn get_hl_funding_payments_by_raw_tx(
        &self,
        raw_tx_id: Uuid,
    ) -> anyhow::Result<Vec<HlFundingPayment>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, coin, amount, funding_rate, \
             payment_time, dataset_version_id, created_at \
             FROM hl_funding_payments WHERE raw_transaction_id = $1 ORDER BY payment_time",
        )
        .bind(raw_tx_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_hl_funding_payment).collect()
    }

    // -----------------------------------------------------------------------
    // HlPositionChanges (P3-W4)
    // -----------------------------------------------------------------------

    /// Bulk insert Hyperliquid position change records.
    pub async fn save_hl_position_changes(
        &self,
        changes: &[HlPositionChange],
    ) -> anyhow::Result<()> {
        for chunk in changes.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_hl_position_change_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Query Hyperliquid position changes by coin.
    pub async fn get_hl_position_changes_by_coin(
        &self,
        coin: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<HlPositionChange>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, coin, side, size_delta, price, \
             direction, source_event, dataset_version_id, created_at \
             FROM hl_position_changes \
             WHERE coin = $1 \
             ORDER BY created_at DESC LIMIT $2 OFFSET $3",
        )
        .bind(coin)
        .bind(limit)
        .bind(offset)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_hl_position_change).collect()
    }

    /// Query Hyperliquid position changes by raw transaction ID.
    pub async fn get_hl_position_changes_by_raw_tx(
        &self,
        raw_tx_id: Uuid,
    ) -> anyhow::Result<Vec<HlPositionChange>> {
        let rows = sqlx::query(
            "SELECT id, raw_transaction_id, network, coin, side, size_delta, price, \
             direction, source_event, dataset_version_id, created_at \
             FROM hl_position_changes WHERE raw_transaction_id = $1 ORDER BY created_at",
        )
        .bind(raw_tx_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_hl_position_change).collect()
    }

    // -----------------------------------------------------------------------
    // DatasetCompleteness (P3-W6)
    // -----------------------------------------------------------------------

    /// Upsert a dataset completeness record.
    /// On conflict (target_id, dataset_name, network), all mutable fields are
    /// updated from the incoming record.
    pub async fn upsert_dataset_completeness(
        &self,
        dc: &DatasetCompleteness,
    ) -> anyhow::Result<()> {
        let (query, args) = build_dataset_completeness_upsert(dc)?;
        sqlx::query_with(&query, args).execute(self.pool()).await?;
        Ok(())
    }

    /// Get a single completeness record by (target_id, dataset_name, network).
    pub async fn get_dataset_completeness(
        &self,
        target_id: Uuid,
        dataset_name: &str,
        network: &str,
    ) -> anyhow::Result<Option<DatasetCompleteness>> {
        let row = sqlx::query(
            "SELECT id, target_id, dataset_name, dataset_version_id, network, status, \
             coverage_start, coverage_end, block_start, block_end, \
             last_ingestion_run_id, records_count, gap_ranges, notes, created_at, updated_at \
             FROM dataset_completeness \
             WHERE target_id = $1 AND dataset_name = $2 AND network = $3",
        )
        .bind(target_id)
        .bind(dataset_name)
        .bind(network)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_dataset_completeness).transpose()
    }

    /// List all completeness records for a given target.
    pub async fn list_completeness_by_target(
        &self,
        target_id: Uuid,
    ) -> anyhow::Result<Vec<DatasetCompleteness>> {
        let rows = sqlx::query(
            "SELECT id, target_id, dataset_name, dataset_version_id, network, status, \
             coverage_start, coverage_end, block_start, block_end, \
             last_ingestion_run_id, records_count, gap_ranges, notes, created_at, updated_at \
             FROM dataset_completeness \
             WHERE target_id = $1 ORDER BY dataset_name, network",
        )
        .bind(target_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_dataset_completeness).collect()
    }

    /// List all target completeness records for a given dataset name.
    pub async fn list_completeness_by_dataset(
        &self,
        dataset_name: &str,
    ) -> anyhow::Result<Vec<DatasetCompleteness>> {
        let rows = sqlx::query(
            "SELECT id, target_id, dataset_name, dataset_version_id, network, status, \
             coverage_start, coverage_end, block_start, block_end, \
             last_ingestion_run_id, records_count, gap_ranges, notes, created_at, updated_at \
             FROM dataset_completeness \
             WHERE dataset_name = $1 ORDER BY target_id, network",
        )
        .bind(dataset_name)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_dataset_completeness).collect()
    }

    /// List completeness records for a dataset with optional target and network filters.
    pub async fn list_completeness_filtered(
        &self,
        dataset_name: &str,
        target_id: Option<Uuid>,
        network: Option<&str>,
    ) -> anyhow::Result<Vec<DatasetCompleteness>> {
        let cols = "id, target_id, dataset_name, dataset_version_id, network, status, \
                    coverage_start, coverage_end, block_start, block_end, \
                    last_ingestion_run_id, records_count, gap_ranges, notes, created_at, updated_at";
        let mut sql = format!("SELECT {cols} FROM dataset_completeness WHERE dataset_name = $1");
        let mut param_idx = 2u32;

        if target_id.is_some() {
            sql.push_str(&format!(" AND target_id = ${param_idx}"));
            param_idx += 1;
        }
        if network.is_some() {
            sql.push_str(&format!(" AND network = ${param_idx}"));
        }
        sql.push_str(" ORDER BY target_id, network");

        let mut query = sqlx::query(&sql).bind(dataset_name);
        if let Some(tid) = target_id {
            query = query.bind(tid);
        }
        if let Some(net) = network {
            query = query.bind(net);
        }

        let rows = query.fetch_all(self.pool()).await?;
        rows.iter().map(row_to_dataset_completeness).collect()
    }

    /// List all completeness records matching a given status.
    pub async fn list_completeness_by_status(
        &self,
        status: CompletenessStatus,
    ) -> anyhow::Result<Vec<DatasetCompleteness>> {
        let rows = sqlx::query(
            "SELECT id, target_id, dataset_name, dataset_version_id, network, status, \
             coverage_start, coverage_end, block_start, block_end, \
             last_ingestion_run_id, records_count, gap_ranges, notes, created_at, updated_at \
             FROM dataset_completeness \
             WHERE status = $1 ORDER BY dataset_name, target_id, network",
        )
        .bind(completeness_status_to_sql(&status))
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_dataset_completeness).collect()
    }

    // -----------------------------------------------------------------------
    // Dataset record queries with target/network/time-window filters (P4-W1)
    // -----------------------------------------------------------------------

    /// Query token transfers with optional target, network, and time-window filters.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_token_transfers(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<TokenTransfer>> {
        let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.token_address, dt.token_symbol, \
                    dt.from_address, dt.to_address, dt.amount, dt.decimals, dt.transfer_index, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::TokenTransfers.physical_table(),
            "dt.created_at",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_token_transfer).collect()
    }

    /// Query native balance deltas with optional target, network, and time-window filters.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_native_balance_deltas(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<NativeBalanceDelta>> {
        let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.account_address, dt.native_token, \
                    dt.pre_balance, dt.post_balance, dt.delta, dt.is_fee_payer, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::NativeBalanceDeltas.physical_table(),
            "dt.created_at",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_native_balance_delta).collect()
    }

    /// Query decoded events with optional target, network, and time-window filters.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_decoded_events(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<DecodedEvent>> {
        let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.program_or_contract, dt.event_signature, \
                    dt.event_name, dt.log_index, dt.decoded_fields, dt.raw_fields, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::DecodedEvents.physical_table(),
            "dt.created_at",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_decoded_event).collect()
    }

    /// Query Hyperliquid fill records with optional target, network, and time-window filters.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_hl_fill_records(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<HlFillRecord>> {
        let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.coin, dt.side, dt.price, dt.size, dt.direction, \
                    dt.closed_pnl, dt.fee, dt.fee_token, dt.fill_time, dt.order_id, dt.trade_id, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::HlFills.physical_table(),
            "dt.fill_time",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_hl_fill_record).collect()
    }

    /// Query Hyperliquid funding payments with optional target, network, and time-window filters.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_hl_funding_payments(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<HlFundingPayment>> {
        let cols =
            "dt.id, dt.raw_transaction_id, dt.network, dt.coin, dt.amount, dt.funding_rate, \
                    dt.payment_time, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::HlFunding.physical_table(),
            "dt.payment_time",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_hl_funding_payment).collect()
    }

    /// Query Hyperliquid position changes with optional target, network, and time-window filters.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_hl_position_changes(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<HlPositionChange>> {
        let cols =
            "dt.id, dt.raw_transaction_id, dt.network, dt.coin, dt.side, dt.size_delta, dt.price, \
                    dt.direction, dt.source_event, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::Positions.physical_table(),
            "dt.created_at",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_hl_position_change).collect()
    }

    // -----------------------------------------------------------------------
    // Export queries — higher limit for batch export jobs (P4-W2)
    // -----------------------------------------------------------------------

    /// Maximum records per export job chunk (100k is practical for in-memory
    /// serialization of Silver rows without exhausting API memory).
    const EXPORT_MAX_RECORDS: i64 = 100_000;

    /// Query token transfers for export with a high record limit.
    #[allow(clippy::too_many_arguments)]
    pub async fn export_token_transfers(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<TokenTransfer>> {
        self.query_token_transfers(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    /// Query native balance deltas for export with a high record limit.
    #[allow(clippy::too_many_arguments)]
    pub async fn export_native_balance_deltas(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<NativeBalanceDelta>> {
        self.query_native_balance_deltas(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    /// Query decoded events for export with a high record limit.
    #[allow(clippy::too_many_arguments)]
    pub async fn export_decoded_events(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<DecodedEvent>> {
        self.query_decoded_events(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    /// Query Hyperliquid fill records for export with a high record limit.
    #[allow(clippy::too_many_arguments)]
    pub async fn export_hl_fill_records(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<HlFillRecord>> {
        self.query_hl_fill_records(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    /// Query Hyperliquid funding payments for export with a high record limit.
    #[allow(clippy::too_many_arguments)]
    pub async fn export_hl_funding_payments(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<HlFundingPayment>> {
        self.query_hl_funding_payments(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    /// Query Hyperliquid position changes for export with a high record limit.
    #[allow(clippy::too_many_arguments)]
    pub async fn export_hl_position_changes(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<HlPositionChange>> {
        self.query_hl_position_changes(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    /// Update completeness after an ingestion run completes.
    /// Expands the coverage window, updates status, records_count, and
    /// last_ingestion_run_id for the given (target_id, dataset_name, network).
    #[allow(clippy::too_many_arguments)]
    pub async fn update_completeness_after_run(
        &self,
        target_id: Uuid,
        dataset_name: &str,
        network: &str,
        status: CompletenessStatus,
        coverage_start: Option<i64>,
        coverage_end: Option<i64>,
        block_start: Option<i64>,
        block_end: Option<i64>,
        records_count: i64,
        ingestion_run_id: Uuid,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE dataset_completeness SET \
                 status = $4, \
                 coverage_start = CASE \
                     WHEN $5::BIGINT IS NULL THEN coverage_start \
                     WHEN coverage_start IS NULL THEN $5 \
                     WHEN $5 < coverage_start THEN $5 \
                     ELSE coverage_start END, \
                 coverage_end = CASE \
                     WHEN $6::BIGINT IS NULL THEN coverage_end \
                     WHEN coverage_end IS NULL THEN $6 \
                     WHEN $6 > coverage_end THEN $6 \
                     ELSE coverage_end END, \
                 block_start = CASE \
                     WHEN $7::BIGINT IS NULL THEN block_start \
                     WHEN block_start IS NULL THEN $7 \
                     WHEN $7 < block_start THEN $7 \
                     ELSE block_start END, \
                 block_end = CASE \
                     WHEN $8::BIGINT IS NULL THEN block_end \
                     WHEN block_end IS NULL THEN $8 \
                     WHEN $8 > block_end THEN $8 \
                     ELSE block_end END, \
                 records_count = $9, \
                 last_ingestion_run_id = $10, \
                 updated_at = NOW() \
             WHERE target_id = $1 AND dataset_name = $2 AND network = $3",
        )
        .bind(target_id)
        .bind(dataset_name)
        .bind(network)
        .bind(completeness_status_to_sql(&status))
        .bind(coverage_start)
        .bind(coverage_end)
        .bind(block_start)
        .bind(block_end)
        .bind(records_count)
        .bind(ingestion_run_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Gold-tier: wallet_ledger and balance_history (P5-W1)
    // -----------------------------------------------------------------------

    /// Upsert wallet_ledger records. Uses ON CONFLICT DO NOTHING on the primary key
    /// to maintain idempotency.
    pub async fn save_wallet_ledger_records(
        &self,
        records: &[WalletLedgerRecord],
    ) -> anyhow::Result<()> {
        for chunk in records.chunks(500) {
            let mut query_builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
                "INSERT INTO wallet_ledger (id, raw_transaction_id, wallet_address, network, tx_hash, \
                 timestamp, entry_type, asset_symbol, amount, counterparty_address, \
                 fee_amount, fee_asset, cost_basis, proceeds, dataset_version_id, created_at) ",
            );
            query_builder.push_values(chunk, |mut b, r| {
                b.push_bind(r.id)
                    .push_bind(r.raw_transaction_id)
                    .push_bind(&r.wallet_address)
                    .push_bind(&r.network)
                    .push_bind(&r.tx_hash)
                    .push_bind(r.timestamp)
                    .push_bind(&r.entry_type)
                    .push_bind(&r.asset_symbol)
                    .push_bind(&r.amount)
                    .push_bind(&r.counterparty_address)
                    .push_bind(&r.fee_amount)
                    .push_bind(&r.fee_asset)
                    .push_bind(&r.cost_basis)
                    .push_bind(&r.proceeds)
                    .push_bind(r.dataset_version_id)
                    .push_bind(r.created_at);
            });
            query_builder.push(
                " ON CONFLICT (id) DO UPDATE SET \
                 amount = EXCLUDED.amount, \
                 counterparty_address = EXCLUDED.counterparty_address, \
                 fee_amount = EXCLUDED.fee_amount, \
                 fee_asset = EXCLUDED.fee_asset, \
                 cost_basis = EXCLUDED.cost_basis, \
                 proceeds = EXCLUDED.proceeds, \
                 dataset_version_id = EXCLUDED.dataset_version_id",
            );
            query_builder.build().execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Upsert balance_history records.
    pub async fn save_balance_snapshots(&self, records: &[BalanceSnapshot]) -> anyhow::Result<()> {
        for chunk in records.chunks(500) {
            let mut query_builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
                "INSERT INTO balance_history (id, wallet_address, asset_symbol, network, \
                 timestamp, balance, tx_hash, dataset_version_id, created_at) ",
            );
            query_builder.push_values(chunk, |mut b, r| {
                b.push_bind(r.id)
                    .push_bind(&r.wallet_address)
                    .push_bind(&r.asset_symbol)
                    .push_bind(&r.network)
                    .push_bind(r.timestamp)
                    .push_bind(&r.balance)
                    .push_bind(&r.tx_hash)
                    .push_bind(r.dataset_version_id)
                    .push_bind(r.created_at);
            });
            query_builder.push(
                " ON CONFLICT (id) DO UPDATE SET \
                 balance = EXCLUDED.balance, \
                 dataset_version_id = EXCLUDED.dataset_version_id",
            );
            query_builder.build().execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Query wallet_ledger records with optional wallet, network, and time-window filters.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_wallet_ledger_records(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<WalletLedgerRecord>> {
        let cols = "dt.id, dt.raw_transaction_id, dt.wallet_address, dt.network, dt.tx_hash, \
                    dt.timestamp, dt.entry_type, dt.asset_symbol, dt.amount, dt.counterparty_address, \
                    dt.fee_amount, dt.fee_asset, dt.cost_basis, dt.proceeds, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::WalletLedger.physical_table(),
            "dt.timestamp",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_wallet_ledger_record).collect()
    }

    /// Query wallet_ledger for export with a high record limit.
    pub async fn export_wallet_ledger_records(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<WalletLedgerRecord>> {
        self.query_wallet_ledger_records(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    /// Query balance_history records with optional wallet, network, and time-window filters.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_balance_snapshots(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<BalanceSnapshot>> {
        let cols = "dt.id, dt.wallet_address, dt.asset_symbol, dt.network, dt.timestamp, \
                    dt.balance, dt.tx_hash, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::BalanceHistory.physical_table(),
            "dt.timestamp",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_balance_snapshot).collect()
    }

    /// Query balance_history for export with a high record limit.
    pub async fn export_balance_snapshots(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<BalanceSnapshot>> {
        self.query_balance_snapshots(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    // -- P5-W2: Hyperliquid Gold analytics query/export methods --

    pub async fn query_hl_pnl_summary(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<HlPnlSummary>> {
        let cols = "dt.id, dt.wallet_address, dt.coin, dt.network, dt.period_start, \
                    dt.period_end, dt.total_closed_pnl, dt.total_funding, dt.total_fees, \
                    dt.net_pnl, dt.trade_count, dt.fill_count, dt.avg_trade_size, \
                    dt.win_count, dt.loss_count, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::HlPnlSummary.physical_table(),
            "dt.period_end",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_hl_pnl_summary).collect()
    }

    pub async fn export_hl_pnl_summary(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<HlPnlSummary>> {
        self.query_hl_pnl_summary(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    pub async fn query_hl_trade_history(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<HlTradeHistory>> {
        let cols = "dt.id, dt.wallet_address, dt.coin, dt.network, dt.side, dt.entry_price, \
                    dt.exit_price, dt.size, dt.opened_at, dt.closed_at, dt.realized_pnl, \
                    dt.fees, dt.num_fills, dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::HlTradeHistory.physical_table(),
            "dt.closed_at",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_hl_trade_history).collect()
    }

    pub async fn export_hl_trade_history(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<HlTradeHistory>> {
        self.query_hl_trade_history(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    // -- P5-W3: Protocol / TVL Gold dataset query/export methods --

    pub async fn query_protocol_events(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<ProtocolEvent>> {
        let cols = "dt.id, dt.network, dt.protocol_address, dt.protocol_name, dt.event_type, \
                    dt.event_details, dt.pool_address, dt.raw_event_id, dt.timestamp, \
                    dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::ProtocolEvents.physical_table(),
            "dt.timestamp",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_protocol_event).collect()
    }

    pub async fn export_protocol_events(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<ProtocolEvent>> {
        self.query_protocol_events(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    pub async fn query_pool_snapshots(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<PoolSnapshot>> {
        let cols = "dt.id, dt.network, dt.pool_address, dt.protocol_address, dt.protocol_name, \
                    dt.token0_address, dt.token0_symbol, dt.token1_address, dt.token1_symbol, \
                    dt.reserve0, dt.reserve1, dt.tvl_usd, dt.snapshot_timestamp, dt.block_number, \
                    dt.dataset_version_id, dt.created_at";
        let (sql, args) = build_dataset_filter_query(
            cols,
            DatasetName::PoolSnapshots.physical_table(),
            "dt.snapshot_timestamp",
            target_id,
            network,
            time_start,
            time_end,
            limit,
            offset,
        )?;
        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_pool_snapshot).collect()
    }

    pub async fn export_pool_snapshots(
        &self,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
    ) -> anyhow::Result<Vec<PoolSnapshot>> {
        self.query_pool_snapshots(
            target_id,
            network,
            time_start,
            time_end,
            Self::EXPORT_MAX_RECORDS,
            0,
        )
        .await
    }

    // -----------------------------------------------------------------------
    // Gold-tier save methods (P5-W4)
    // -----------------------------------------------------------------------

    /// Bulk insert HL PnL summary records.
    pub async fn save_hl_pnl_summary(&self, records: &[HlPnlSummary]) -> anyhow::Result<()> {
        for chunk in records.chunks(500) {
            let mut query_builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
                "INSERT INTO hl_pnl_summary (id, wallet_address, coin, network, period_start, \
                 period_end, total_closed_pnl, total_funding, total_fees, net_pnl, trade_count, \
                 fill_count, avg_trade_size, win_count, loss_count, dataset_version_id, created_at) ",
            );
            query_builder.push_values(chunk, |mut b, r| {
                b.push_bind(r.id)
                    .push_bind(&r.wallet_address)
                    .push_bind(&r.coin)
                    .push_bind(&r.network)
                    .push_bind(r.period_start)
                    .push_bind(r.period_end)
                    .push_bind(&r.total_closed_pnl)
                    .push_bind(&r.total_funding)
                    .push_bind(&r.total_fees)
                    .push_bind(&r.net_pnl)
                    .push_bind(r.trade_count)
                    .push_bind(r.fill_count)
                    .push_bind(&r.avg_trade_size)
                    .push_bind(r.win_count)
                    .push_bind(r.loss_count)
                    .push_bind(r.dataset_version_id)
                    .push_bind(r.created_at);
            });
            query_builder.push(" ON CONFLICT (id) DO NOTHING");
            query_builder.build().execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Bulk insert HL trade history records.
    pub async fn save_hl_trade_history(&self, records: &[HlTradeHistory]) -> anyhow::Result<()> {
        for chunk in records.chunks(500) {
            let mut query_builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
                "INSERT INTO hl_trade_history (id, wallet_address, coin, network, side, \
                 entry_price, exit_price, size, opened_at, closed_at, realized_pnl, fees, \
                 num_fills, dataset_version_id, created_at) ",
            );
            query_builder.push_values(chunk, |mut b, r| {
                b.push_bind(r.id)
                    .push_bind(&r.wallet_address)
                    .push_bind(&r.coin)
                    .push_bind(&r.network)
                    .push_bind(&r.side)
                    .push_bind(&r.entry_price)
                    .push_bind(&r.exit_price)
                    .push_bind(&r.size)
                    .push_bind(r.opened_at)
                    .push_bind(r.closed_at)
                    .push_bind(&r.realized_pnl)
                    .push_bind(&r.fees)
                    .push_bind(r.num_fills)
                    .push_bind(r.dataset_version_id)
                    .push_bind(r.created_at);
            });
            query_builder.push(" ON CONFLICT (id) DO NOTHING");
            query_builder.build().execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Bulk insert protocol event records.
    pub async fn save_protocol_events(&self, records: &[ProtocolEvent]) -> anyhow::Result<()> {
        for chunk in records.chunks(500) {
            let mut query_builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
                "INSERT INTO protocol_events (id, network, protocol_address, protocol_name, \
                 event_type, event_details, pool_address, raw_event_id, timestamp, \
                 dataset_version_id, created_at) ",
            );
            query_builder.push_values(chunk, |mut b, r| {
                b.push_bind(r.id)
                    .push_bind(&r.network)
                    .push_bind(&r.protocol_address)
                    .push_bind(&r.protocol_name)
                    .push_bind(&r.event_type)
                    .push_bind(&r.event_details)
                    .push_bind(&r.pool_address)
                    .push_bind(r.raw_event_id)
                    .push_bind(r.timestamp)
                    .push_bind(r.dataset_version_id)
                    .push_bind(r.created_at);
            });
            query_builder.push(" ON CONFLICT (id) DO NOTHING");
            query_builder.build().execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Bulk insert pool snapshot records.
    pub async fn save_pool_snapshots(&self, records: &[PoolSnapshot]) -> anyhow::Result<()> {
        for chunk in records.chunks(500) {
            let mut query_builder: sqlx::QueryBuilder<sqlx::Postgres> = sqlx::QueryBuilder::new(
                "INSERT INTO pool_snapshots (id, network, pool_address, protocol_address, \
                 protocol_name, token0_address, token0_symbol, token1_address, token1_symbol, \
                 reserve0, reserve1, tvl_usd, snapshot_timestamp, block_number, \
                 dataset_version_id, created_at) ",
            );
            query_builder.push_values(chunk, |mut b, r| {
                b.push_bind(r.id)
                    .push_bind(&r.network)
                    .push_bind(&r.pool_address)
                    .push_bind(&r.protocol_address)
                    .push_bind(&r.protocol_name)
                    .push_bind(&r.token0_address)
                    .push_bind(&r.token0_symbol)
                    .push_bind(&r.token1_address)
                    .push_bind(&r.token1_symbol)
                    .push_bind(&r.reserve0)
                    .push_bind(&r.reserve1)
                    .push_bind(&r.tvl_usd)
                    .push_bind(r.snapshot_timestamp)
                    .push_bind(r.block_number)
                    .push_bind(r.dataset_version_id)
                    .push_bind(r.created_at);
            });
            query_builder.push(" ON CONFLICT (id) DO NOTHING");
            query_builder.build().execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Query the latest balance snapshot per (wallet, asset, network).
    /// Used to seed running balances before incremental balance_history computation.
    pub async fn get_latest_balance_snapshots(
        &self,
        wallet_address: &str,
        network: &str,
    ) -> anyhow::Result<Vec<BalanceSnapshot>> {
        let rows = sqlx::query(
            "SELECT DISTINCT ON (wallet_address, asset_symbol, network) \
             id, wallet_address, asset_symbol, network, timestamp, balance, tx_hash, \
             dataset_version_id, created_at \
             FROM balance_history \
             WHERE wallet_address = $1 AND network = $2 \
             ORDER BY wallet_address, asset_symbol, network, timestamp DESC, created_at DESC, id DESC",
        )
        .bind(wallet_address)
        .bind(network)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_balance_snapshot).collect()
    }

    // -----------------------------------------------------------------------
    // Snapshotted streaming export (fixes PR #238 [P1] correctness)
    // -----------------------------------------------------------------------

    /// Stream a snapshotted, paged export for one dataset into `tx_out`.
    ///
    /// Runs every page query inside a single `REPEATABLE READ READ ONLY`
    /// transaction so pagination is stable even if ingestion writes rows
    /// concurrently — the reader sees a single snapshot for the duration
    /// of the export. Combined with the `(order_col, dt.id)` total ORDER
    /// BY (see [`build_dataset_filter_query`]), this guarantees every
    /// page is disjoint and together they cover the full result set
    /// exactly once. This fixes the PR #238 [P1] comment that the
    /// previous per-page queries could skip or duplicate rows on tied
    /// `created_at`/`timestamp` values or under concurrent insert.
    ///
    /// Cancellation: checks `cancel` before every query and every send.
    /// When the heartbeat task detects that the export job's lease was
    /// reclaimed by another worker (the PR #238 [P1] lease-loss issue),
    /// it cancels this token; the function aborts, the transaction is
    /// rolled back on drop, and the caller can clean up its attempt-
    /// scoped temp artifact without racing the new owner.
    ///
    /// Backpressure: the per-page channel is expected to be small
    /// (`capacity=2` in the worker) so that DB streaming throttles to
    /// the disk-write rate rather than buffering whole pages in RAM.
    ///
    /// Returns the total number of records sent.
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_export_snapshot(
        &self,
        dataset: &str,
        target_id: Option<Uuid>,
        network: Option<&str>,
        time_start: Option<i64>,
        time_end: Option<i64>,
        page_size: i64,
        hard_cap: i64,
        cancel: tokio_util::sync::CancellationToken,
        tx_out: tokio::sync::mpsc::Sender<ExportRecordBatch>,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool().begin().await?;
        // Snapshot isolation: every page sees the same committed state
        // that existed when this SET statement returned. READ ONLY lets
        // Postgres skip some write-path bookkeeping.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;

        let total = match dataset {
            "token_transfers" => {
                let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.token_address, \
                            dt.token_symbol, dt.from_address, dt.to_address, dt.amount, \
                            dt.decimals, dt.transfer_index, dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::TokenTransfers.physical_table(),
                    "dt.created_at",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_token_transfer,
                    ExportRecordBatch::TokenTransfers,
                )
                .await?
            }
            "native_balance_deltas" => {
                let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.account_address, \
                            dt.native_token, dt.pre_balance, dt.post_balance, dt.delta, \
                            dt.is_fee_payer, dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::NativeBalanceDeltas.physical_table(),
                    "dt.created_at",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_native_balance_delta,
                    ExportRecordBatch::NativeBalanceDeltas,
                )
                .await?
            }
            "decoded_events" => {
                let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.program_or_contract, \
                            dt.event_signature, dt.event_name, dt.log_index, dt.decoded_fields, \
                            dt.raw_fields, dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::DecodedEvents.physical_table(),
                    "dt.created_at",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_decoded_event,
                    ExportRecordBatch::DecodedEvents,
                )
                .await?
            }
            "hl_fills" => {
                let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.coin, dt.side, \
                            dt.price, dt.size, dt.direction, dt.closed_pnl, dt.fee, \
                            dt.fee_token, dt.fill_time, dt.order_id, dt.trade_id, \
                            dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::HlFills.physical_table(),
                    "dt.fill_time",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_hl_fill_record,
                    ExportRecordBatch::HlFills,
                )
                .await?
            }
            "hl_funding" => {
                let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.coin, dt.amount, \
                            dt.funding_rate, dt.payment_time, dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::HlFunding.physical_table(),
                    "dt.payment_time",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_hl_funding_payment,
                    ExportRecordBatch::HlFunding,
                )
                .await?
            }
            "positions" => {
                let cols = "dt.id, dt.raw_transaction_id, dt.network, dt.coin, dt.side, \
                            dt.size_delta, dt.price, dt.direction, dt.source_event, \
                            dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::Positions.physical_table(),
                    "dt.created_at",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_hl_position_change,
                    ExportRecordBatch::Positions,
                )
                .await?
            }
            "wallet_ledger" => {
                let cols = "dt.id, dt.raw_transaction_id, dt.wallet_address, dt.network, \
                            dt.tx_hash, dt.timestamp, dt.entry_type, dt.asset_symbol, \
                            dt.amount, dt.counterparty_address, dt.fee_amount, dt.fee_asset, \
                            dt.cost_basis, dt.proceeds, dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::WalletLedger.physical_table(),
                    "dt.timestamp",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_wallet_ledger_record,
                    ExportRecordBatch::WalletLedger,
                )
                .await?
            }
            "balance_history" => {
                let cols = "dt.id, dt.wallet_address, dt.asset_symbol, dt.network, \
                            dt.timestamp, dt.balance, dt.tx_hash, dt.dataset_version_id, \
                            dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::BalanceHistory.physical_table(),
                    "dt.timestamp",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_balance_snapshot,
                    ExportRecordBatch::BalanceHistory,
                )
                .await?
            }
            "hl_pnl_summary" => {
                let cols = "dt.id, dt.wallet_address, dt.coin, dt.network, dt.period_start, \
                            dt.period_end, dt.total_closed_pnl, dt.total_funding, dt.total_fees, \
                            dt.net_pnl, dt.trade_count, dt.fill_count, dt.avg_trade_size, \
                            dt.win_count, dt.loss_count, dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::HlPnlSummary.physical_table(),
                    "dt.period_end",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_hl_pnl_summary,
                    ExportRecordBatch::HlPnlSummary,
                )
                .await?
            }
            "hl_trade_history" => {
                let cols = "dt.id, dt.wallet_address, dt.coin, dt.network, dt.side, \
                            dt.entry_price, dt.exit_price, dt.size, dt.opened_at, dt.closed_at, \
                            dt.realized_pnl, dt.fees, dt.num_fills, dt.dataset_version_id, \
                            dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::HlTradeHistory.physical_table(),
                    "dt.closed_at",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_hl_trade_history,
                    ExportRecordBatch::HlTradeHistory,
                )
                .await?
            }
            "protocol_events" => {
                let cols = "dt.id, dt.network, dt.protocol_address, dt.protocol_name, \
                            dt.event_type, dt.event_details, dt.pool_address, dt.raw_event_id, \
                            dt.timestamp, dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::ProtocolEvents.physical_table(),
                    "dt.timestamp",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_protocol_event,
                    ExportRecordBatch::ProtocolEvents,
                )
                .await?
            }
            "pool_snapshots" => {
                let cols = "dt.id, dt.network, dt.pool_address, dt.protocol_address, \
                            dt.protocol_name, dt.token0_address, dt.token0_symbol, \
                            dt.token1_address, dt.token1_symbol, dt.reserve0, dt.reserve1, \
                            dt.tvl_usd, dt.snapshot_timestamp, dt.block_number, \
                            dt.dataset_version_id, dt.created_at";
                stream_paged_in_tx(
                    &mut tx,
                    cols,
                    DatasetName::PoolSnapshots.physical_table(),
                    "dt.snapshot_timestamp",
                    target_id,
                    network,
                    time_start,
                    time_end,
                    page_size,
                    hard_cap,
                    &cancel,
                    &tx_out,
                    row_to_pool_snapshot,
                    ExportRecordBatch::PoolSnapshots,
                )
                .await?
            }
            other => {
                // Rolling back the snapshot transaction on unknown dataset
                // is implicit via drop.
                return Err(anyhow::anyhow!("Unknown export dataset: {other}"));
            }
        };

        tx.commit().await?;
        Ok(total)
    }

    // -----------------------------------------------------------------------
    // Raw EVM Traces (Bronze)
    // -----------------------------------------------------------------------

    /// Bulk insert raw EVM trace records.
    pub async fn save_raw_evm_traces(&self, traces: &[RawEvmTrace]) -> anyhow::Result<()> {
        for chunk in traces.chunks(Self::V2_BATCH_SIZE) {
            let (query, args) = build_raw_evm_trace_insert(chunk)?;
            sqlx::query_with(&query, args).execute(self.pool()).await?;
        }
        Ok(())
    }

    /// Fetch a raw EVM trace by network, transaction hash, and trace type.
    pub async fn get_raw_evm_trace(
        &self,
        network: &str,
        transaction_hash: &str,
        trace_type: EvmTraceType,
    ) -> anyhow::Result<Option<RawEvmTrace>> {
        let row = sqlx::query(
            "SELECT id, transaction_hash, block_number, network, trace_type, \
             raw_trace, ingestion_run_id, created_at \
             FROM raw_evm_traces \
             WHERE network = $1 AND transaction_hash = $2 AND trace_type = $3",
        )
        .bind(network)
        .bind(transaction_hash)
        .bind(trace_type.to_string())
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_raw_evm_trace).transpose()
    }

    /// Fetch all raw EVM traces for a block range (inclusive).
    pub async fn get_raw_evm_traces_by_block_range(
        &self,
        network: &str,
        from_block: i64,
        to_block: i64,
    ) -> anyhow::Result<Vec<RawEvmTrace>> {
        let rows = sqlx::query(
            "SELECT id, transaction_hash, block_number, network, trace_type, \
             raw_trace, ingestion_run_id, created_at \
             FROM raw_evm_traces \
             WHERE network = $1 AND block_number >= $2 AND block_number <= $3 \
             ORDER BY block_number",
        )
        .bind(network)
        .bind(from_block)
        .bind(to_block)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_raw_evm_trace).collect()
    }

    // -----------------------------------------------------------------------
    // Ingestion Jobs (Durable Control Plane)
    // -----------------------------------------------------------------------

    /// Enqueue an ingestion job with status=pending.
    ///
    /// If `idempotency_key` is set and a job with that key already exists,
    /// the existing job is returned instead of creating a duplicate.
    ///
    /// Uses atomic `INSERT ... ON CONFLICT DO NOTHING` to avoid races
    /// between concurrent enqueue requests sharing the same key.
    pub async fn enqueue_ingestion_job(
        &self,
        params: &EnqueueIngestionJobParams<'_>,
    ) -> anyhow::Result<IngestionJob> {
        let id = Uuid::new_v4();

        if params.idempotency_key.is_some() {
            // Atomic path: attempt insert; on conflict the unique partial
            // index on idempotency_key causes DO NOTHING, returning zero
            // rows.  We then SELECT the pre-existing row.
            let maybe_row = sqlx::query(
                "INSERT INTO ingestion_jobs \
                 (id, target_id, network, mode, status, priority, idempotency_key, \
                  requested_by, callback_url) \
                 VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7, $8) \
                 ON CONFLICT (idempotency_key) WHERE idempotency_key IS NOT NULL \
                 DO NOTHING \
                 RETURNING id, target_id, network, mode, status, priority, idempotency_key, \
                           requested_by, callback_url, error_message, created_at, updated_at",
            )
            .bind(id)
            .bind(params.target_id)
            .bind(params.network)
            .bind(params.mode)
            .bind(params.priority)
            .bind(params.idempotency_key)
            .bind(params.requested_by)
            .bind(params.callback_url)
            .fetch_optional(self.pool())
            .await?;

            if let Some(row) = maybe_row {
                // Insert succeeded (new row).
                return row_to_ingestion_job(&row);
            }

            // Conflict: the idempotency key already exists.  Return the
            // existing row.
            let existing = sqlx::query(
                "SELECT id, target_id, network, mode, status, priority, idempotency_key, \
                 requested_by, callback_url, error_message, created_at, updated_at \
                 FROM ingestion_jobs WHERE idempotency_key = $1",
            )
            .bind(params.idempotency_key)
            .fetch_one(self.pool())
            .await?;
            return row_to_ingestion_job(&existing);
        }

        // No idempotency key: plain insert.
        let row = sqlx::query(
            "INSERT INTO ingestion_jobs \
             (id, target_id, network, mode, status, priority, idempotency_key, \
              requested_by, callback_url) \
             VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7, $8) \
             RETURNING id, target_id, network, mode, status, priority, idempotency_key, \
                       requested_by, callback_url, error_message, created_at, updated_at",
        )
        .bind(id)
        .bind(params.target_id)
        .bind(params.network)
        .bind(params.mode)
        .bind(params.priority)
        .bind(params.idempotency_key)
        .bind(params.requested_by)
        .bind(params.callback_url)
        .fetch_one(self.pool())
        .await?;
        row_to_ingestion_job(&row)
    }

    /// Claim the next available ingestion job using `FOR UPDATE SKIP LOCKED`.
    ///
    /// Atomically transitions the job to `claimed` status and creates an
    /// attempt record. Considers both `pending` jobs and stale
    /// `claimed`/`running` jobs whose latest attempt heartbeat has exceeded
    /// [`Self::STALE_JOB_THRESHOLD_MINUTES`], recovering work abandoned by
    /// dead workers. Returns `None` if no claimable jobs are available.
    pub async fn claim_ingestion_job(
        &self,
        worker_id: &str,
    ) -> anyhow::Result<Option<IngestionJob>> {
        let mut tx = self.pool().begin().await?;

        // Find and lock the highest-priority claimable job.
        // A job is claimable if it is pending, OR if it is claimed/running
        // but its latest attempt heartbeat is stale (dead worker recovery).
        let maybe_row = sqlx::query(
            "SELECT j.id, j.target_id, j.network, j.mode, j.status, j.priority, \
                    j.idempotency_key, j.requested_by, j.callback_url, j.error_message, \
                    j.created_at, j.updated_at \
             FROM ingestion_jobs j \
             WHERE j.status = 'pending' \
                OR (j.status IN ('claimed', 'running') \
                    AND NOT EXISTS ( \
                        SELECT 1 FROM ingestion_job_attempts a \
                        WHERE a.job_id = j.id AND a.finished_at IS NULL \
                          AND a.heartbeat_at > NOW() - make_interval(mins => $1) \
                    )) \
             ORDER BY j.priority DESC, j.created_at ASC \
             LIMIT 1 \
             FOR UPDATE OF j SKIP LOCKED",
        )
        .bind(Self::STALE_JOB_THRESHOLD_MINUTES)
        .fetch_optional(&mut *tx)
        .await?;

        let row = match maybe_row {
            Some(r) => r,
            None => return Ok(None),
        };

        let job_id: Uuid = row.try_get("id")?;

        // Mark any open attempts from a previous (dead) worker as failed.
        sqlx::query(
            "UPDATE ingestion_job_attempts \
             SET finished_at = NOW(), error_message = 'reclaimed: worker presumed dead' \
             WHERE job_id = $1 AND finished_at IS NULL",
        )
        .bind(job_id)
        .execute(&mut *tx)
        .await?;

        // Transition to claimed.
        sqlx::query(
            "UPDATE ingestion_jobs SET status = 'claimed', updated_at = NOW() WHERE id = $1",
        )
        .bind(job_id)
        .execute(&mut *tx)
        .await?;

        // Create attempt record.
        sqlx::query(
            "INSERT INTO ingestion_job_attempts (job_id, worker_id, attempt_num) \
             VALUES ($1, $2, \
               COALESCE((SELECT MAX(attempt_num) FROM ingestion_job_attempts WHERE job_id = $1), 0) + 1)",
        )
        .bind(job_id)
        .bind(worker_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;

        // Re-fetch with updated status.
        self.get_ingestion_job(job_id).await.map(|opt| {
            opt.map(|mut j| {
                j.status = IngestionJobStatus::Claimed;
                j
            })
        })
    }

    /// Record a heartbeat for an in-progress ingestion job.
    ///
    /// Updates both the job's `updated_at` and the latest attempt's
    /// `heartbeat_at` for the given worker.
    pub async fn heartbeat_ingestion_job(
        &self,
        job_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<bool> {
        sqlx::query("UPDATE ingestion_jobs SET updated_at = NOW() WHERE id = $1")
            .bind(job_id)
            .execute(self.pool())
            .await?;

        let result = sqlx::query(
            "UPDATE ingestion_job_attempts SET heartbeat_at = NOW() \
             WHERE job_id = $1 AND worker_id = $2 AND finished_at IS NULL",
        )
        .bind(job_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;

        // If no rows were updated, the lease was reclaimed by another worker.
        Ok(result.rows_affected() > 0)
    }

    /// Transition an ingestion job from `claimed` to `running`.
    ///
    /// Atomically verifies that `worker_id` owns the current open attempt
    /// inside the same UPDATE statement, preventing TOCTOU races where a
    /// stale worker passes a separate ownership check just before another
    /// worker reclaims the job.
    pub async fn mark_ingestion_job_running(
        &self,
        job_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE ingestion_jobs SET status = 'running', updated_at = NOW() \
             WHERE id = $1 AND status = 'claimed' \
             AND EXISTS( \
                 SELECT 1 FROM ingestion_job_attempts \
                 WHERE job_id = $1 AND worker_id = $2 AND finished_at IS NULL \
             )",
        )
        .bind(job_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            anyhow::bail!(
                "worker {worker_id} does not own the current attempt for job {job_id} \
                 (job may have been reclaimed or is not in 'claimed' status)"
            );
        }
        Ok(())
    }

    /// Mark an ingestion job as completed.
    ///
    /// Atomically verifies that `worker_id` owns the current open attempt
    /// inside the same UPDATE statement, preventing TOCTOU races.
    pub async fn complete_ingestion_job(
        &self,
        job_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE ingestion_jobs \
             SET status = 'completed', updated_at = NOW() \
             WHERE id = $1 AND status IN ('claimed', 'running') \
             AND EXISTS( \
                 SELECT 1 FROM ingestion_job_attempts \
                 WHERE job_id = $1 AND worker_id = $2 AND finished_at IS NULL \
             )",
        )
        .bind(job_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            anyhow::bail!(
                "worker {worker_id} does not own the current attempt for job {job_id} \
                 (job may have been reclaimed); refusing to complete"
            );
        }

        // Close *this* worker's attempt.
        sqlx::query(
            "UPDATE ingestion_job_attempts SET finished_at = NOW() \
             WHERE job_id = $1 AND worker_id = $2 AND finished_at IS NULL",
        )
        .bind(job_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Mark an ingestion job as failed with an error message.
    ///
    /// Atomically verifies that `worker_id` owns the current open attempt
    /// inside the same UPDATE statement, preventing TOCTOU races.
    pub async fn fail_ingestion_job(
        &self,
        job_id: Uuid,
        worker_id: &str,
        error: &str,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE ingestion_jobs \
             SET status = 'failed', error_message = $3, updated_at = NOW() \
             WHERE id = $1 AND status IN ('claimed', 'running') \
             AND EXISTS( \
                 SELECT 1 FROM ingestion_job_attempts \
                 WHERE job_id = $1 AND worker_id = $2 AND finished_at IS NULL \
             )",
        )
        .bind(job_id)
        .bind(worker_id)
        .bind(error)
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            anyhow::bail!(
                "worker {worker_id} does not own the current attempt for job {job_id} \
                 (job may have been reclaimed); refusing to mark failed"
            );
        }

        // Close *this* worker's attempt.
        sqlx::query(
            "UPDATE ingestion_job_attempts \
             SET finished_at = NOW(), error_message = $3 \
             WHERE job_id = $1 AND worker_id = $2 AND finished_at IS NULL",
        )
        .bind(job_id)
        .bind(worker_id)
        .bind(error)
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Get an ingestion job by ID.
    pub async fn get_ingestion_job(&self, id: Uuid) -> anyhow::Result<Option<IngestionJob>> {
        let row = sqlx::query(
            "SELECT id, target_id, network, mode, status, priority, idempotency_key, \
             requested_by, callback_url, error_message, created_at, updated_at \
             FROM ingestion_jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_ingestion_job).transpose()
    }

    /// List ingestion jobs with optional status filter.
    pub async fn list_ingestion_jobs(
        &self,
        status: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<IngestionJob>> {
        let rows = match status {
            Some(s) => {
                sqlx::query(
                    "SELECT id, target_id, network, mode, status, priority, idempotency_key, \
                     requested_by, callback_url, error_message, created_at, updated_at \
                     FROM ingestion_jobs WHERE status = $1 \
                     ORDER BY priority DESC, created_at ASC LIMIT $2 OFFSET $3",
                )
                .bind(s)
                .bind(limit)
                .bind(offset)
                .fetch_all(self.pool())
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, target_id, network, mode, status, priority, idempotency_key, \
                     requested_by, callback_url, error_message, created_at, updated_at \
                     FROM ingestion_jobs \
                     ORDER BY created_at DESC LIMIT $1 OFFSET $2",
                )
                .bind(limit)
                .bind(offset)
                .fetch_all(self.pool())
                .await?
            }
        };
        rows.iter().map(row_to_ingestion_job).collect()
    }

    // -----------------------------------------------------------------------
    // Stream Subscriptions (Durable Control Plane)
    // -----------------------------------------------------------------------

    /// Create or update a stream subscription.
    ///
    /// If a subscription already exists for the same (target_id, network, source),
    /// updates the desired_status and config.
    pub async fn upsert_stream_subscription(
        &self,
        target_id: Option<Uuid>,
        network: &str,
        source: &str,
        desired_status: &str,
        config: Option<&serde_json::Value>,
    ) -> anyhow::Result<StreamSubscription> {
        let id = Uuid::new_v4();
        let row = sqlx::query(
            "INSERT INTO stream_subscriptions \
             (id, target_id, network, source, desired_status, config) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (target_id, network, source) WHERE target_id IS NOT NULL \
             DO UPDATE SET \
               desired_status = EXCLUDED.desired_status, \
               config = COALESCE(EXCLUDED.config, stream_subscriptions.config), \
               updated_at = NOW() \
             RETURNING id, target_id, network, source, desired_status, actual_status, \
                       lease_owner, heartbeat_at, cursor_state, config, error_message, \
                       created_at, updated_at",
        )
        .bind(id)
        .bind(target_id)
        .bind(network)
        .bind(source)
        .bind(desired_status)
        .bind(config)
        .fetch_one(self.pool())
        .await?;
        row_to_stream_subscription(&row)
    }

    /// Claim a stream subscription lease for a worker.
    ///
    /// Uses `FOR UPDATE SKIP LOCKED` to atomically acquire the lease.
    /// Only claims subscriptions where `desired_status = 'active'` and
    /// either no current owner or the lease has expired (stale heartbeat).
    pub async fn claim_stream_lease(
        &self,
        subscription_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE stream_subscriptions \
             SET lease_owner = $2, actual_status = 'running', \
                 heartbeat_at = NOW(), updated_at = NOW() \
             WHERE id = $1 \
               AND desired_status = 'active' \
               AND (lease_owner IS NULL OR heartbeat_at < NOW() - INTERVAL '5 minutes')",
        )
        .bind(subscription_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Record a heartbeat for an active stream subscription.
    pub async fn heartbeat_stream(
        &self,
        subscription_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE stream_subscriptions \
             SET heartbeat_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND lease_owner = $2",
        )
        .bind(subscription_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Release a stream subscription lease, setting actual_status to stopped.
    ///
    /// Ownership-aware: only clears the lease if `worker_id` matches the
    /// current `lease_owner`, preventing a stale worker from releasing
    /// another worker's active lease. Returns `true` if the release
    /// succeeded, `false` if the lease was already held by someone else.
    pub async fn release_stream_lease(
        &self,
        subscription_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE stream_subscriptions \
             SET lease_owner = NULL, actual_status = 'stopped', \
                 heartbeat_at = NULL, updated_at = NOW() \
             WHERE id = $1 AND lease_owner = $2",
        )
        .bind(subscription_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update `desired_status` for a stream subscription by ID.
    ///
    /// Used by stop_stream to express user intent without needing the
    /// full upsert key (target_id, network, source).
    pub async fn set_stream_desired_status(
        &self,
        subscription_id: Uuid,
        desired_status: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE stream_subscriptions \
             SET desired_status = $2, updated_at = NOW() \
             WHERE id = $1",
        )
        .bind(subscription_id)
        .bind(desired_status)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Transition a stream subscription to error state with ownership check.
    ///
    /// Sets `actual_status = 'error'` **and** `desired_status = 'stopped'`,
    /// records the error message, and clears the lease. Only succeeds if
    /// the caller owns the lease.
    ///
    /// Setting `desired_status = 'stopped'` is critical: without it the row
    /// stays `desired_status = 'active'` with no lease owner, which makes it
    /// immediately claimable again and turns a fatal failure into a tight
    /// retry loop. The user must explicitly re-activate the subscription
    /// after investigating the error.
    pub async fn fail_stream_subscription(
        &self,
        subscription_id: Uuid,
        worker_id: &str,
        error_message: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE stream_subscriptions \
             SET actual_status = 'error', desired_status = 'stopped', \
                 error_message = $3, \
                 lease_owner = NULL, heartbeat_at = NULL, updated_at = NOW() \
             WHERE id = $1 AND lease_owner = $2",
        )
        .bind(subscription_id)
        .bind(worker_id)
        .bind(error_message)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List stream subscriptions that are eligible to be claimed.
    ///
    /// Returns subscriptions where `desired_status = 'active'` and either
    /// no lease owner or the heartbeat has gone stale. The orchestrator
    /// should then attempt `claim_stream_lease` for each returned row.
    pub async fn list_claimable_stream_subscriptions(
        &self,
        limit: i64,
    ) -> anyhow::Result<Vec<StreamSubscription>> {
        let rows = sqlx::query(
            "SELECT id, target_id, network, source, desired_status, actual_status, \
             lease_owner, heartbeat_at, cursor_state, config, error_message, \
             created_at, updated_at \
             FROM stream_subscriptions \
             WHERE desired_status = 'active' \
               AND (lease_owner IS NULL \
                    OR heartbeat_at < NOW() - make_interval(mins => $1)) \
             ORDER BY created_at ASC \
             LIMIT $2",
        )
        .bind(Self::STALE_JOB_THRESHOLD_MINUTES)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_stream_subscription).collect()
    }

    /// Update the cursor state for a stream subscription.
    pub async fn update_stream_cursor(
        &self,
        subscription_id: Uuid,
        cursor_state: &serde_json::Value,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE stream_subscriptions \
             SET cursor_state = $2, updated_at = NOW() \
             WHERE id = $1",
        )
        .bind(subscription_id)
        .bind(cursor_state)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Get a stream subscription by ID.
    pub async fn get_stream_subscription(
        &self,
        id: Uuid,
    ) -> anyhow::Result<Option<StreamSubscription>> {
        let row = sqlx::query(
            "SELECT id, target_id, network, source, desired_status, actual_status, \
             lease_owner, heartbeat_at, cursor_state, config, error_message, \
             created_at, updated_at \
             FROM stream_subscriptions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_stream_subscription).transpose()
    }

    /// List stream subscriptions with optional desired_status filter.
    pub async fn list_stream_subscriptions(
        &self,
        desired_status: Option<&str>,
        owner_id: Option<Uuid>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<StreamSubscription>> {
        let rows = if let Some(oid) = owner_id {
            // Owner-scoped: join with index_targets to enforce ownership.
            match desired_status {
                Some(s) => {
                    sqlx::query(
                        "SELECT s.id, s.target_id, s.network, s.source, s.desired_status, s.actual_status, \
                         s.lease_owner, s.heartbeat_at, s.cursor_state, s.config, s.error_message, \
                         s.created_at, s.updated_at \
                         FROM stream_subscriptions s \
                         JOIN index_targets t ON t.id = s.target_id \
                         WHERE s.desired_status = $1 AND t.owner_id = $2 \
                         ORDER BY s.created_at LIMIT $3 OFFSET $4",
                    )
                    .bind(s)
                    .bind(oid)
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(self.pool())
                    .await?
                }
                None => {
                    sqlx::query(
                        "SELECT s.id, s.target_id, s.network, s.source, s.desired_status, s.actual_status, \
                         s.lease_owner, s.heartbeat_at, s.cursor_state, s.config, s.error_message, \
                         s.created_at, s.updated_at \
                         FROM stream_subscriptions s \
                         JOIN index_targets t ON t.id = s.target_id \
                         WHERE t.owner_id = $1 \
                         ORDER BY s.created_at LIMIT $2 OFFSET $3",
                    )
                    .bind(oid)
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(self.pool())
                    .await?
                }
            }
        } else {
            match desired_status {
                Some(s) => {
                    sqlx::query(
                        "SELECT id, target_id, network, source, desired_status, actual_status, \
                         lease_owner, heartbeat_at, cursor_state, config, error_message, \
                         created_at, updated_at \
                         FROM stream_subscriptions WHERE desired_status = $1 \
                         ORDER BY created_at LIMIT $2 OFFSET $3",
                    )
                    .bind(s)
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(self.pool())
                    .await?
                }
                None => {
                    sqlx::query(
                        "SELECT id, target_id, network, source, desired_status, actual_status, \
                         lease_owner, heartbeat_at, cursor_state, config, error_message, \
                         created_at, updated_at \
                         FROM stream_subscriptions \
                         ORDER BY created_at LIMIT $1 OFFSET $2",
                    )
                    .bind(limit)
                    .bind(offset)
                    .fetch_all(self.pool())
                    .await?
                }
            }
        };
        rows.iter().map(row_to_stream_subscription).collect()
    }

    // -----------------------------------------------------------------------
    // Export Jobs (Durable Control Plane)
    // -----------------------------------------------------------------------

    /// Enqueue an export job with status=pending.
    pub async fn enqueue_export_job(
        &self,
        dataset: DatasetName,
        format: ExportFormat,
        filters: Option<&serde_json::Value>,
        sink_config: Option<&serde_json::Value>,
        owner_id: Option<Uuid>,
    ) -> anyhow::Result<ExportJob> {
        let id = Uuid::new_v4();
        let row = sqlx::query(
            "INSERT INTO export_jobs \
             (id, dataset, format, filters, sink_config, owner_id) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             RETURNING id, dataset, format, filters, sink_config, status, \
                       worker_id, record_count, result_location, \
                       delivery_destination, error_message, \
                       dataset_version_id, dataset_version, completeness_status, \
                       completeness_coverage, last_ingestion_run_id, \
                       created_at, updated_at, started_at, completed_at, heartbeat_at, owner_id",
        )
        .bind(id)
        .bind(dataset.to_string())
        .bind(format.to_string())
        .bind(filters)
        .bind(sink_config)
        .bind(owner_id)
        .fetch_one(self.pool())
        .await?;
        row_to_export_job(&row)
    }

    /// Claim the next available export job using `FOR UPDATE SKIP LOCKED`.
    ///
    /// Considers `pending` jobs and stale `running` or `delivering` jobs whose
    /// `heartbeat_at` has exceeded [`Self::STALE_JOB_THRESHOLD_MINUTES`],
    /// recovering work abandoned by dead workers at any in-progress phase.
    pub async fn claim_export_job(&self, worker_id: &str) -> anyhow::Result<Option<ExportJob>> {
        let mut tx = self.pool().begin().await?;

        let maybe_row = sqlx::query(
            "SELECT id FROM export_jobs \
             WHERE status = 'pending' \
                OR (status IN ('running', 'delivering') \
                    AND (heartbeat_at IS NULL OR heartbeat_at < NOW() - make_interval(mins => $1))) \
             ORDER BY created_at ASC \
             LIMIT 1 \
             FOR UPDATE SKIP LOCKED",
        )
        .bind(Self::STALE_JOB_THRESHOLD_MINUTES)
        .fetch_optional(&mut *tx)
        .await?;

        let row = match maybe_row {
            Some(r) => r,
            None => return Ok(None),
        };

        let job_id: Uuid = row.try_get("id")?;

        sqlx::query(
            "UPDATE export_jobs \
             SET status = 'running', worker_id = $2, \
                 started_at = COALESCE(started_at, NOW()), \
                 heartbeat_at = NOW(), updated_at = NOW() \
             WHERE id = $1",
        )
        .bind(job_id)
        .bind(worker_id)
        .execute(&mut *tx)
        .await?;

        let updated = sqlx::query(
            "SELECT id, dataset, format, filters, sink_config, status, \
             worker_id, record_count, result_location, \
             delivery_destination, error_message, \
             dataset_version_id, dataset_version, completeness_status, \
             completeness_coverage, last_ingestion_run_id, \
             created_at, updated_at, started_at, completed_at, heartbeat_at \
             FROM export_jobs WHERE id = $1",
        )
        .bind(job_id)
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(row_to_export_job(&updated)?))
    }

    /// Update the status (and optional fields) of an export job.
    ///
    /// The caller must supply the `worker_id` that currently owns the lease.
    /// The UPDATE is guarded by `AND worker_id = $7 AND status IN ('running', 'delivering')`
    /// so that a stale worker whose lease was reclaimed cannot clobber the new
    /// owner's progress.  Returns `Ok(true)` when the row was updated, or
    /// `Ok(false)` when the lease was lost (row not matched).
    #[allow(clippy::too_many_arguments)]
    pub async fn update_export_job_status(
        &self,
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
    ) -> anyhow::Result<bool> {
        let completed_at = if status == "completed" || status == "failed" {
            Some(Utc::now())
        } else {
            None
        };

        let result = sqlx::query(
            "UPDATE export_jobs \
             SET status = $2, record_count = COALESCE($3, record_count), \
                 result_location = COALESCE($4, result_location), \
                 error_message = COALESCE($5, error_message), \
                 completed_at = COALESCE($6, completed_at), \
                 delivery_destination = COALESCE($8, delivery_destination), \
                 dataset_version_id = COALESCE($9, dataset_version_id), \
                 dataset_version = COALESCE($10, dataset_version), \
                 completeness_status = COALESCE($11, completeness_status), \
                 completeness_coverage = COALESCE($12, completeness_coverage), \
                 last_ingestion_run_id = COALESCE($13, last_ingestion_run_id), \
                 updated_at = NOW() \
             WHERE id = $1 AND worker_id = $7 AND status IN ('running', 'delivering')",
        )
        .bind(job_id)
        .bind(status)
        .bind(record_count)
        .bind(result_location)
        .bind(error_message)
        .bind(completed_at)
        .bind(worker_id)
        .bind(delivery_destination)
        .bind(dataset_version_id)
        .bind(dataset_version)
        .bind(completeness_status)
        .bind(completeness_coverage)
        .bind(last_ingestion_run_id)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Record a heartbeat for an in-progress export job.
    ///
    /// Updates `heartbeat_at` and `updated_at` for the given job, but only
    /// if the caller is the current `worker_id` owner and the job is in an
    /// active phase (`running` or `delivering`). Prevents a stale worker
    /// from extending its lease after reclaim.
    pub async fn heartbeat_export_job(
        &self,
        job_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE export_jobs SET heartbeat_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND worker_id = $2 AND status IN ('running', 'delivering')",
        )
        .bind(job_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Get an export job by ID.
    pub async fn get_export_job(&self, id: Uuid) -> anyhow::Result<Option<ExportJob>> {
        let row = sqlx::query(
            "SELECT id, dataset, format, filters, sink_config, status, \
             worker_id, record_count, result_location, \
             delivery_destination, error_message, \
             dataset_version_id, dataset_version, completeness_status, \
             completeness_coverage, last_ingestion_run_id, \
             created_at, updated_at, started_at, completed_at, heartbeat_at \
             FROM export_jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_export_job).transpose()
    }

    // -----------------------------------------------------------------------
    // Materialization Runs (Durable Control Plane)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // API Keys (Tenant Isolation — Issue #216)
    // -----------------------------------------------------------------------

    /// Insert a new API key. The caller is responsible for hashing the key.
    pub async fn create_api_key(
        &self,
        key_hash: &str,
        name: Option<&str>,
        owner_id: Uuid,
    ) -> anyhow::Result<ApiKey> {
        let row = sqlx::query(
            "INSERT INTO api_keys (key_hash, name, owner_id) \
             VALUES ($1, $2, $3) \
             RETURNING id, key_hash, name, owner_id, created_at, revoked_at",
        )
        .bind(key_hash)
        .bind(name)
        .bind(owner_id)
        .fetch_one(self.pool())
        .await?;
        row_to_api_key(&row)
    }

    /// Validate an API key by its raw hash. Returns `Ok(None)` if the key
    /// does not exist or has been revoked.
    pub async fn validate_api_key(&self, key_hash: &str) -> anyhow::Result<Option<ApiKey>> {
        let row = sqlx::query(
            "SELECT id, key_hash, name, owner_id, created_at, revoked_at \
             FROM api_keys \
             WHERE key_hash = $1 AND revoked_at IS NULL",
        )
        .bind(key_hash)
        .fetch_optional(self.pool())
        .await?;
        match row {
            Some(r) => Ok(Some(row_to_api_key(&r)?)),
            None => Ok(None),
        }
    }

    /// Revoke an API key by id, scoped to an owner.
    pub async fn revoke_api_key(&self, id: Uuid, owner_id: Uuid) -> anyhow::Result<()> {
        sqlx::query("UPDATE api_keys SET revoked_at = NOW() WHERE id = $1 AND owner_id = $2")
            .bind(id)
            .bind(owner_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// List API keys for a given owner (excluding revoked).
    pub async fn list_api_keys_by_owner(&self, owner_id: Uuid) -> anyhow::Result<Vec<ApiKey>> {
        let rows = sqlx::query(
            "SELECT id, key_hash, name, owner_id, created_at, revoked_at \
             FROM api_keys \
             WHERE owner_id = $1 AND revoked_at IS NULL \
             ORDER BY created_at",
        )
        .bind(owner_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_api_key).collect()
    }

    /// List wallet targets belonging to a specific owner (no pagination limit).
    pub async fn list_wallet_targets_by_owner(
        &self,
        owner_id: Uuid,
    ) -> anyhow::Result<Vec<IndexTarget>> {
        let rows = sqlx::query(
            "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
             mode::text, label, owner_id, created_at, updated_at \
             FROM index_targets \
             WHERE kind = 'wallet'::target_kind_enum AND owner_id = $1",
        )
        .bind(owner_id)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_index_target).collect()
    }

    /// Create a new materialization run with status=pending.
    pub async fn create_materialization_run(
        &self,
        dataset_name: &str,
        scope: Option<&serde_json::Value>,
        input_watermark: Option<i64>,
        dataset_version_id: Option<Uuid>,
        worker_id: Option<&str>,
    ) -> anyhow::Result<MaterializationRun> {
        let id = Uuid::new_v4();
        let row = sqlx::query(
            "INSERT INTO materialization_runs \
             (id, dataset_name, scope, input_watermark, status, \
              dataset_version_id, worker_id) \
             VALUES ($1, $2, $3, $4, 'pending', $5, $6) \
             RETURNING id, dataset_name, scope, input_watermark, output_record_count, \
                       status, dataset_version_id, worker_id, started_at, finished_at, \
                       heartbeat_at, error_message, created_at, updated_at",
        )
        .bind(id)
        .bind(dataset_name)
        .bind(scope)
        .bind(input_watermark)
        .bind(dataset_version_id)
        .bind(worker_id)
        .fetch_one(self.pool())
        .await?;
        row_to_materialization_run(&row)
    }

    /// Atomically claim a materialization run for a worker.
    /// Succeeds if status is 'pending' or the heartbeat is stale (dead worker recovery).
    pub async fn claim_materialization_run(
        &self,
        run_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE materialization_runs \
             SET status = 'running', worker_id = $2, started_at = COALESCE(started_at, NOW()), \
                 heartbeat_at = NOW(), updated_at = NOW() \
             WHERE id = $1 \
               AND (status = 'pending' \
                    OR (status = 'running' AND (heartbeat_at IS NULL OR heartbeat_at < NOW() - INTERVAL '5 minutes')))",
        )
        .bind(run_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Update the heartbeat timestamp for a running materialization run.
    pub async fn heartbeat_materialization_run(
        &self,
        run_id: Uuid,
        worker_id: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE materialization_runs \
             SET heartbeat_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND worker_id = $2 AND status = 'running'",
        )
        .bind(run_id)
        .bind(worker_id)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Mark a materialization run as completed with output stats.
    pub async fn complete_materialization_run(
        &self,
        run_id: Uuid,
        worker_id: &str,
        output_record_count: i64,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE materialization_runs \
             SET status = 'completed', output_record_count = $2, \
                 finished_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND worker_id = $3 AND status = 'running'",
        )
        .bind(run_id)
        .bind(output_record_count)
        .bind(worker_id)
        .execute(self.pool())
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!("complete_materialization_run: 0 rows affected (ownership lost or invalid state) for run_id={run_id}");
        }
        Ok(())
    }

    /// Mark a materialization run as failed.
    pub async fn fail_materialization_run(
        &self,
        run_id: Uuid,
        worker_id: &str,
        error: &str,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE materialization_runs \
             SET status = 'failed', error_message = $2, \
                 finished_at = NOW(), updated_at = NOW() \
             WHERE id = $1 AND worker_id = $3 AND status = 'running'",
        )
        .bind(run_id)
        .bind(error)
        .bind(worker_id)
        .execute(self.pool())
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!("fail_materialization_run: 0 rows affected (ownership lost or invalid state) for run_id={run_id}");
        }
        Ok(())
    }

    /// Get a materialization run by ID.
    pub async fn get_materialization_run(
        &self,
        id: Uuid,
    ) -> anyhow::Result<Option<MaterializationRun>> {
        let row = sqlx::query(
            "SELECT id, dataset_name, scope, input_watermark, output_record_count, \
             status, dataset_version_id, worker_id, started_at, finished_at, \
             heartbeat_at, error_message, created_at, updated_at \
             FROM materialization_runs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_materialization_run).transpose()
    }

    /// List materialization runs that are claimable: pending or stale-heartbeat running.
    pub async fn list_claimable_materialization_runs(
        &self,
        limit: i64,
    ) -> anyhow::Result<Vec<MaterializationRun>> {
        let rows = sqlx::query(
            "SELECT id, dataset_name, scope, input_watermark, output_record_count, \
             status, dataset_version_id, worker_id, started_at, finished_at, \
             heartbeat_at, error_message, created_at, updated_at \
             FROM materialization_runs \
             WHERE status = 'pending' \
                OR (status = 'running' AND (heartbeat_at IS NULL OR heartbeat_at < NOW() - INTERVAL '5 minutes')) \
             ORDER BY created_at ASC \
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        rows.iter().map(row_to_materialization_run).collect()
    }

    // -----------------------------------------------------------------------
    // V2-backed wallet reads (Workstream F compatibility cutover)
    // -----------------------------------------------------------------------

    /// Fetch transactions for a wallet from V2 Bronze (raw_transactions + target_matches).
    /// Returns V2 RawTransaction rows matched to the wallet's target(s).
    pub async fn get_wallet_transactions_v2(
        &self,
        wallet: &str,
        owner_id: Option<Uuid>,
        limit: i64,
        offset: i64,
        from: Option<i64>,
        to: Option<i64>,
    ) -> anyhow::Result<Vec<RawTransaction>> {
        let limit = limit.min(MAX_QUERY_LIMIT);
        let mut sql = String::from(
            "SELECT DISTINCT rt.id, rt.network, rt.tx_hash, rt.timestamp, rt.block_number, \
             rt.raw_metadata, rt.source, rt.ingestion_run_id, rt.ingested_at \
             FROM raw_transactions rt \
             JOIN target_matches tm ON tm.raw_transaction_id = rt.id \
             JOIN index_targets it ON it.id = tm.target_id \
             WHERE it.address = $1 AND it.kind = 'wallet'",
        );
        let mut args = sqlx::postgres::PgArguments::default();
        use sqlx::Arguments;
        args.add(wallet.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut n: usize = 1;

        if let Some(oid) = owner_id {
            n += 1;
            sql.push_str(&format!(" AND it.owner_id = ${n}"));
            args.add(oid).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        if let Some(start) = from {
            n += 1;
            sql.push_str(&format!(" AND rt.timestamp >= ${n}"));
            args.add(start).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        if let Some(end) = to {
            n += 1;
            sql.push_str(&format!(" AND rt.timestamp <= ${n}"));
            args.add(end).map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        n += 1;
        let lim_n = n;
        n += 1;
        let off_n = n;
        sql.push_str(&format!(
            " ORDER BY rt.timestamp ASC LIMIT ${lim_n} OFFSET ${off_n}"
        ));
        args.add(limit).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(offset).map_err(|e| anyhow::anyhow!("{e}"))?;

        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_raw_transaction).collect()
    }

    /// Fetch a single transaction by wallet + tx_hash from V2 Bronze.
    pub async fn get_wallet_transaction_by_hash_v2(
        &self,
        wallet: &str,
        tx_hash: &str,
        owner_id: Option<Uuid>,
    ) -> anyhow::Result<Option<RawTransaction>> {
        let mut sql = String::from(
            "SELECT DISTINCT rt.id, rt.network, rt.tx_hash, rt.timestamp, rt.block_number, \
             rt.raw_metadata, rt.source, rt.ingestion_run_id, rt.ingested_at \
             FROM raw_transactions rt \
             JOIN target_matches tm ON tm.raw_transaction_id = rt.id \
             JOIN index_targets it ON it.id = tm.target_id \
             WHERE it.address = $1 AND it.kind = 'wallet' AND rt.tx_hash = $2",
        );
        let mut args = sqlx::postgres::PgArguments::default();
        use sqlx::Arguments;
        args.add(wallet.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(tx_hash.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut n: usize = 2;

        if let Some(oid) = owner_id {
            n += 1;
            sql.push_str(&format!(" AND it.owner_id = ${n}"));
            args.add(oid).map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        let row = sqlx::query_with(&sql, args)
            .fetch_optional(self.pool())
            .await?;
        row.as_ref().map(row_to_raw_transaction).transpose()
    }

    /// Fetch ledger entries for a wallet from V2 wallet_ledger.
    pub async fn get_wallet_ledger_v2(
        &self,
        wallet: &str,
        owner_id: Option<Uuid>,
        limit: i64,
        offset: i64,
        from: Option<i64>,
        to: Option<i64>,
    ) -> anyhow::Result<Vec<WalletLedgerRecord>> {
        // Defense-in-depth: reject tenant requests for unowned wallets.
        // TODO: add owner_id to wallet_ledger for true row-level isolation.
        if let Some(oid) = owner_id {
            let owned: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM index_targets WHERE kind = 'wallet' AND address = $1 AND owner_id = $2"
            )
            .bind(wallet)
            .bind(oid)
            .fetch_one(self.pool())
            .await?;
            if owned.0 == 0 {
                return Ok(vec![]);
            }
        }

        let limit = limit.min(MAX_QUERY_LIMIT);
        let mut sql = String::from(
            "SELECT id, raw_transaction_id, wallet_address, network, tx_hash, \
             timestamp, entry_type, asset_symbol, amount, counterparty_address, \
             fee_amount, fee_asset, cost_basis, proceeds, dataset_version_id, created_at \
             FROM wallet_ledger WHERE wallet_address = $1",
        );
        let mut args = sqlx::postgres::PgArguments::default();
        use sqlx::Arguments;
        args.add(wallet.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut n: usize = 1;

        if let Some(start) = from {
            n += 1;
            sql.push_str(&format!(" AND timestamp >= ${n}"));
            args.add(start).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        if let Some(end) = to {
            n += 1;
            sql.push_str(&format!(" AND timestamp <= ${n}"));
            args.add(end).map_err(|e| anyhow::anyhow!("{e}"))?;
        }

        n += 1;
        let lim_n = n;
        n += 1;
        let off_n = n;
        sql.push_str(&format!(
            " ORDER BY timestamp ASC LIMIT ${lim_n} OFFSET ${off_n}"
        ));
        args.add(limit).map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(offset).map_err(|e| anyhow::anyhow!("{e}"))?;

        let rows = sqlx::query_with(&sql, args).fetch_all(self.pool()).await?;
        rows.iter().map(row_to_wallet_ledger_record).collect()
    }

    /// Aggregate current balances for a wallet from V2 wallet_ledger.
    /// Optionally filter by point-in-time timestamp.
    pub async fn get_wallet_balances_v2(
        &self,
        wallet: &str,
        owner_id: Option<Uuid>,
        at: Option<i64>,
    ) -> anyhow::Result<Vec<(String, bigdecimal::BigDecimal)>> {
        // Defense-in-depth: reject tenant requests for unowned wallets.
        // TODO: add owner_id to wallet_ledger for true row-level isolation.
        if let Some(oid) = owner_id {
            let owned: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM index_targets WHERE kind = 'wallet' AND address = $1 AND owner_id = $2"
            )
            .bind(wallet)
            .bind(oid)
            .fetch_one(self.pool())
            .await?;
            if owned.0 == 0 {
                return Ok(vec![]);
            }
        }

        let rows = if let Some(at_ts) = at {
            sqlx::query(
                "SELECT asset_symbol, SUM(amount) as balance \
                 FROM wallet_ledger \
                 WHERE wallet_address = $1 AND timestamp <= $2 \
                 GROUP BY asset_symbol HAVING SUM(amount) != 0 \
                 ORDER BY asset_symbol",
            )
            .bind(wallet)
            .bind(at_ts)
            .fetch_all(self.pool())
            .await?
        } else {
            sqlx::query(
                "SELECT asset_symbol, SUM(amount) as balance \
                 FROM wallet_ledger \
                 WHERE wallet_address = $1 \
                 GROUP BY asset_symbol HAVING SUM(amount) != 0 \
                 ORDER BY asset_symbol",
            )
            .bind(wallet)
            .fetch_all(self.pool())
            .await?
        };

        use sqlx::Row;
        rows.iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("asset_symbol")?,
                    row.try_get::<bigdecimal::BigDecimal, _>("balance")?,
                ))
            })
            .collect()
    }

    /// Get wallet stats from V2 tables (raw_transactions + target_matches + wallet_ledger).
    pub async fn get_wallet_stats_v2(
        &self,
        wallet: &str,
        owner_id: Option<Uuid>,
    ) -> anyhow::Result<WalletStatsV2> {
        use sqlx::Row;

        // Defense-in-depth: reject tenant requests for unowned wallets.
        // TODO: add owner_id to wallet_ledger for true row-level isolation.
        if let Some(oid) = owner_id {
            let owned: (i64,) = sqlx::query_as(
                "SELECT COUNT(*) FROM index_targets WHERE kind = 'wallet' AND address = $1 AND owner_id = $2"
            )
            .bind(wallet)
            .bind(oid)
            .fetch_one(self.pool())
            .await?;
            if owned.0 == 0 {
                return Ok(WalletStatsV2 {
                    tx_count: 0,
                    earliest_timestamp: None,
                    latest_timestamp: None,
                    network_count: 0,
                    unique_assets: 0,
                    per_network: vec![],
                });
            }
        }

        // Transaction counts from raw_transactions via target_matches
        let mut tx_sql = String::from(
            "SELECT COUNT(DISTINCT rt.id) AS tx_count, \
                    MIN(rt.timestamp) AS earliest_timestamp, \
                    MAX(rt.timestamp) AS latest_timestamp, \
                    COUNT(DISTINCT rt.network) AS network_count \
             FROM raw_transactions rt \
             JOIN target_matches tm ON tm.raw_transaction_id = rt.id \
             JOIN index_targets it ON it.id = tm.target_id \
             WHERE it.address = $1 AND it.kind = 'wallet'",
        );
        let mut tx_args = sqlx::postgres::PgArguments::default();
        use sqlx::Arguments;
        tx_args
            .add(wallet.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut n: usize = 1;
        if let Some(oid) = owner_id {
            n += 1;
            tx_sql.push_str(&format!(" AND it.owner_id = ${n}"));
            tx_args.add(oid).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        let tx_row = sqlx::query_with(&tx_sql, tx_args)
            .fetch_one(self.pool())
            .await?;

        let tx_count: i64 = tx_row.try_get("tx_count")?;
        let earliest_timestamp: Option<i64> = tx_row.try_get("earliest_timestamp")?;
        let latest_timestamp: Option<i64> = tx_row.try_get("latest_timestamp")?;
        let network_count: i64 = tx_row.try_get("network_count")?;

        // Unique assets from wallet_ledger
        let asset_row = sqlx::query(
            "SELECT COUNT(DISTINCT asset_symbol) AS unique_assets \
             FROM wallet_ledger WHERE wallet_address = $1",
        )
        .bind(wallet)
        .fetch_one(self.pool())
        .await?;
        let unique_assets: i64 = asset_row.try_get("unique_assets")?;

        // Per-network counts
        let mut net_sql = String::from(
            "SELECT rt.network, COUNT(DISTINCT rt.id) AS count \
             FROM raw_transactions rt \
             JOIN target_matches tm ON tm.raw_transaction_id = rt.id \
             JOIN index_targets it ON it.id = tm.target_id \
             WHERE it.address = $1 AND it.kind = 'wallet'",
        );
        let mut net_args = sqlx::postgres::PgArguments::default();
        net_args
            .add(wallet.to_string())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut net_n: usize = 1;
        if let Some(oid) = owner_id {
            net_n += 1;
            net_sql.push_str(&format!(" AND it.owner_id = ${net_n}"));
            net_args.add(oid).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        net_sql.push_str(" GROUP BY rt.network ORDER BY rt.network");
        let net_rows = sqlx::query_with(&net_sql, net_args)
            .fetch_all(self.pool())
            .await?;

        let per_network: Vec<(String, i64)> = net_rows
            .iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("network")?,
                    r.try_get::<i64, _>("count")?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        Ok(WalletStatsV2 {
            tx_count,
            earliest_timestamp,
            latest_timestamp,
            network_count,
            unique_assets,
            per_network,
        })
    }

    // -----------------------------------------------------------------------
    // Dataset Watermarks
    // -----------------------------------------------------------------------

    /// Get the watermark for a dataset + scope combination.
    pub async fn get_dataset_watermark(
        &self,
        dataset_name: &str,
        scope: Option<&serde_json::Value>,
    ) -> anyhow::Result<Option<DatasetWatermark>> {
        let row = sqlx::query(
            "SELECT id, dataset_name, scope, last_ingestion_run_id, last_raw_transaction_id, \
             last_processed_at, created_at, updated_at \
             FROM dataset_watermarks \
             WHERE dataset_name = $1 AND scope IS NOT DISTINCT FROM $2",
        )
        .bind(dataset_name)
        .bind(scope)
        .fetch_optional(self.pool())
        .await?;
        row.as_ref().map(row_to_dataset_watermark).transpose()
    }

    /// Upsert watermark after successful materialization.
    ///
    /// Uses a monotonicity guard based on ingestion run start time: the
    /// watermark is only advanced when the new run started after the run
    /// currently recorded. This prevents out-of-order completions from
    /// regressing the watermark to an older Bronze position.
    pub async fn upsert_dataset_watermark(
        &self,
        dataset_name: &str,
        scope: Option<&serde_json::Value>,
        last_run_id: Option<Uuid>,
        last_raw_tx_id: Option<Uuid>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO dataset_watermarks \
             (dataset_name, scope, last_ingestion_run_id, last_raw_transaction_id, last_processed_at) \
             VALUES ($1, $2, $3, $4, NOW()) \
             ON CONFLICT (dataset_name, scope) DO UPDATE SET \
                 last_ingestion_run_id = EXCLUDED.last_ingestion_run_id, \
                 last_raw_transaction_id = COALESCE(EXCLUDED.last_raw_transaction_id, dataset_watermarks.last_raw_transaction_id), \
                 last_processed_at = NOW(), \
                 updated_at = NOW() \
             WHERE dataset_watermarks.last_ingestion_run_id IS NULL \
                OR (SELECT started_at FROM ingestion_runs WHERE id = EXCLUDED.last_ingestion_run_id) \
                   > COALESCE((SELECT started_at FROM ingestion_runs WHERE id = dataset_watermarks.last_ingestion_run_id), \
                              '1970-01-01'::timestamptz)",
        )
        .bind(dataset_name)
        .bind(scope)
        .bind(last_run_id)
        .bind(last_raw_tx_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

fn row_to_dataset_watermark(row: &sqlx::postgres::PgRow) -> anyhow::Result<DatasetWatermark> {
    Ok(DatasetWatermark {
        id: row.try_get("id")?,
        dataset_name: row.try_get("dataset_name")?,
        scope: row.try_get("scope")?,
        last_ingestion_run_id: row.try_get("last_ingestion_run_id")?,
        last_raw_transaction_id: row.try_get("last_raw_transaction_id")?,
        last_processed_at: row.try_get("last_processed_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    // -- Enum helper roundtrips --

    #[test]
    fn chain_family_sql_roundtrip() {
        for cf in [
            ChainFamily::Solana,
            ChainFamily::Evm,
            ChainFamily::Hyperliquid,
        ] {
            let s = chain_family_to_sql(&cf);
            let back = sql_to_chain_family(s).unwrap();
            assert_eq!(cf, back, "roundtrip failed for {cf:?}");
        }
    }

    #[test]
    fn chain_family_sql_unknown() {
        assert!(sql_to_chain_family("bitcoin").is_err());
    }

    #[test]
    fn target_kind_sql_roundtrip() {
        let all = [
            TargetKind::Wallet,
            TargetKind::Contract,
            TargetKind::Program,
            TargetKind::Account,
            TargetKind::TopicFilter,
            TargetKind::Market,
            TargetKind::Pool,
            TargetKind::Protocol,
        ];
        assert_eq!(all.len(), 8);
        for tk in all {
            let s = target_kind_to_sql(&tk);
            let back = sql_to_target_kind(s).unwrap();
            assert_eq!(tk, back, "roundtrip failed for {tk:?}");
        }
    }

    #[test]
    fn target_kind_sql_unknown() {
        assert!(sql_to_target_kind("nft").is_err());
    }

    #[test]
    fn target_mode_sql_roundtrip() {
        for tm in [TargetMode::Backfill, TargetMode::Stream, TargetMode::Both] {
            let s = target_mode_to_sql(&tm);
            let back = sql_to_target_mode(s).unwrap();
            assert_eq!(tm, back, "roundtrip failed for {tm:?}");
        }
    }

    #[test]
    fn target_mode_sql_unknown() {
        assert!(sql_to_target_mode("realtime").is_err());
    }

    // -- Query builder tests --

    fn make_raw_tx() -> RawTransaction {
        RawTransaction {
            id: Uuid::new_v4(),
            network: "solana-mainnet".to_string(),
            tx_hash: "abc123".to_string(),
            timestamp: 1700000000,
            block_number: Some(200),
            raw_metadata: serde_json::json!({"slot": 200}),
            source: "rpc".to_string(),
            ingestion_run_id: None,
            ingested_at: Utc::now(),
        }
    }

    fn make_target_match() -> TargetMatch {
        TargetMatch {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            raw_transaction_id: Uuid::new_v4(),
            match_reason: Some("sender".to_string()),
            matched_at: Utc::now(),
        }
    }

    fn make_index_target() -> IndexTarget {
        let now = Utc::now();
        IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string()),
            filter_spec: None,
            mode: TargetMode::Both,
            label: Some("test".to_string()),
            owner_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn make_checkpoint() -> Checkpoint {
        Checkpoint {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            network: "solana-mainnet".to_string(),
            source: "grpc".to_string(),
            cursor: serde_json::json!({"last_slot": 300}),
            updated_at: Utc::now(),
        }
    }

    fn make_ingestion_run() -> IngestionRun {
        IngestionRun {
            id: Uuid::new_v4(),
            target_id: Some(Uuid::new_v4()),
            network: "solana-mainnet".to_string(),
            source: "rpc".to_string(),
            mode: IngestionJobMode::Backfill,
            status: IngestionJobStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            records_written: 0,
            error_message: None,
            cursor_state: None,
        }
    }

    fn make_dataset_version() -> DatasetVersion {
        DatasetVersion {
            id: Uuid::new_v4(),
            dataset_name: "ledger_entries".to_string(),
            version: 1,
            parser_hash: Some("sha256:abc".to_string()),
            created_at: Utc::now(),
            notes: None,
            status: DatasetVersionStatus::Active,
        }
    }

    // -- raw_transactions batch insert --

    #[test]
    fn raw_tx_insert_single() {
        let tx = make_raw_tx();
        let (query, _) = build_raw_transaction_insert(&[tx]).unwrap();

        assert!(query.starts_with("INSERT INTO raw_transactions"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        assert!(query.ends_with("ON CONFLICT (network, tx_hash) DO NOTHING"));
    }

    #[test]
    fn raw_tx_insert_multiple() {
        let txs: Vec<_> = (0..3).map(|_| make_raw_tx()).collect();
        let (query, _) = build_raw_transaction_insert(&txs).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        assert!(query.contains("($10, $11, $12, $13, $14, $15, $16, $17, $18)"));
        assert!(query.contains("($19, $20, $21, $22, $23, $24, $25, $26, $27)"));
        assert!(query.ends_with("ON CONFLICT (network, tx_hash) DO NOTHING"));
    }

    #[test]
    fn raw_tx_insert_param_count() {
        let txs: Vec<_> = (0..5).map(|_| make_raw_tx()).collect();
        let (query, _) = build_raw_transaction_insert(&txs).unwrap();
        // 5 rows * 9 params = 45 => highest param is $45
        assert!(query.contains("$45"));
        assert!(!query.contains("$46"));
    }

    // -- raw_transactions upsert returning --

    #[test]
    fn raw_tx_upsert_returning_single() {
        let tx = make_raw_tx();
        let (query, _) = build_raw_transaction_upsert_returning(&[tx]).unwrap();

        assert!(query.starts_with("INSERT INTO raw_transactions"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        assert!(query.ends_with(
            "ON CONFLICT (network, tx_hash) DO UPDATE SET updated_at = NOW() RETURNING id"
        ));
    }

    #[test]
    fn raw_tx_upsert_returning_multiple() {
        let txs: Vec<_> = (0..3).map(|_| make_raw_tx()).collect();
        let (query, _) = build_raw_transaction_upsert_returning(&txs).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        assert!(query.contains("($10, $11, $12, $13, $14, $15, $16, $17, $18)"));
        assert!(query.ends_with(
            "ON CONFLICT (network, tx_hash) DO UPDATE SET updated_at = NOW() RETURNING id"
        ));
    }

    // -- target_matches batch insert --

    #[test]
    fn target_match_insert_single() {
        let m = make_target_match();
        let (query, _) = build_target_match_insert(&[m]).unwrap();

        assert!(query.starts_with("INSERT INTO target_matches"));
        assert!(query.contains("($1, $2, $3, $4, $5)"));
        assert!(query.ends_with("ON CONFLICT (target_id, raw_transaction_id) DO NOTHING"));
    }

    #[test]
    fn target_match_insert_multiple() {
        let matches: Vec<_> = (0..3).map(|_| make_target_match()).collect();
        let (query, _) = build_target_match_insert(&matches).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5)"));
        assert!(query.contains("($6, $7, $8, $9, $10)"));
        assert!(query.contains("($11, $12, $13, $14, $15)"));
        assert!(query.ends_with("ON CONFLICT (target_id, raw_transaction_id) DO NOTHING"));
    }

    // -- index_target insert --

    #[test]
    fn index_target_insert_uses_enum_casts() {
        let t = make_index_target();
        let (query, _) = build_index_target_insert(&t).unwrap();

        assert!(query.contains("$2::target_kind_enum"));
        assert!(query.contains("$4::chain_family_enum"));
        assert!(query.contains("$7::target_mode_enum"));
    }

    #[test]
    fn index_target_insert_has_11_params() {
        let t = make_index_target();
        let (query, _) = build_index_target_insert(&t).unwrap();
        assert!(query.contains("$11"));
        assert!(!query.contains("$12"));
    }

    // -- checkpoint upsert --

    #[test]
    fn checkpoint_upsert_on_conflict() {
        let cp = make_checkpoint();
        let (query, _) = build_checkpoint_upsert(&cp).unwrap();

        assert!(query.contains("INSERT INTO checkpoints"));
        assert!(query.contains("ON CONFLICT (target_id, network, source)"));
        assert!(query.contains("DO UPDATE SET cursor = EXCLUDED.cursor"));
        assert!(query.contains("updated_at = EXCLUDED.updated_at"));
    }

    #[test]
    fn checkpoint_upsert_has_6_params() {
        let cp = make_checkpoint();
        let (query, _) = build_checkpoint_upsert(&cp).unwrap();
        assert!(query.contains("$6"));
        assert!(!query.contains("$7"));
    }

    // -- ingestion_run insert --

    #[test]
    fn ingestion_run_insert_has_11_params() {
        let run = make_ingestion_run();
        let (query, _) = build_ingestion_run_insert(&run).unwrap();

        assert!(query.starts_with("INSERT INTO ingestion_runs"));
        assert!(query.contains("$11"));
        assert!(!query.contains("$12"));
    }

    // -- dataset_version insert --

    #[test]
    fn dataset_version_insert_has_7_params() {
        let dv = make_dataset_version();
        let (query, _) = build_dataset_version_insert(&dv).unwrap();

        assert!(query.starts_with("INSERT INTO dataset_versions"));
        assert!(query.contains("$7"));
        assert!(!query.contains("$8"));
        assert!(query.contains("status"));
    }

    // -- dataset_version_status SQL helpers --

    #[test]
    fn dataset_version_status_sql_roundtrip() {
        for status in [
            DatasetVersionStatus::Active,
            DatasetVersionStatus::Superseded,
            DatasetVersionStatus::Failed,
        ] {
            let s = dataset_version_status_to_sql(&status);
            let back = sql_to_dataset_version_status(s).unwrap();
            assert_eq!(status, back, "roundtrip failed for {status:?}");
        }
    }

    #[test]
    fn dataset_version_status_sql_unknown() {
        assert!(sql_to_dataset_version_status("pending").is_err());
    }

    // -- list_index_targets query format --

    #[test]
    fn list_index_targets_query_uses_limit_offset() {
        // The list_index_targets method uses a fixed query with LIMIT and OFFSET.
        // We verify the method exists with the correct signature at compile time
        // by referencing its type.
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.list_index_targets(10, 0));
        }
        let _ = _check;
    }

    #[test]
    fn list_index_targets_filtered_compiles_all_combos() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            // no filters
            _assert_send(repo.list_index_targets_filtered(None, None, 10, 0));
            // network only
            _assert_send(repo.list_index_targets_filtered(Some("solana-mainnet"), None, 10, 0));
            // kind only
            _assert_send(repo.list_index_targets_filtered(None, Some(TargetKind::Wallet), 10, 0));
            // both filters
            _assert_send(repo.list_index_targets_filtered(
                Some("solana-mainnet"),
                Some(TargetKind::Contract),
                10,
                0,
            ));
        }
        let _ = _check;
    }

    // -- V2 batch size --

    #[test]
    fn v2_batch_size_matches_v1() {
        assert_eq!(Repository::V2_BATCH_SIZE, 500);
    }

    // -- token_transfer batch insert (P3-W2) --

    fn make_token_transfer() -> TokenTransfer {
        use bigdecimal::BigDecimal;
        TokenTransfer {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "ethereum-mainnet".to_string(),
            token_address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
            token_symbol: Some("USDC".to_string()),
            from_address: "0x1111111111111111111111111111111111111111".to_string(),
            to_address: "0x2222222222222222222222222222222222222222".to_string(),
            amount: BigDecimal::from(100),
            decimals: 6,
            transfer_index: 0,
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    fn make_native_balance_delta() -> NativeBalanceDelta {
        use bigdecimal::BigDecimal;
        NativeBalanceDelta {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "solana-mainnet".to_string(),
            account_address: "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string(),
            native_token: "SOL".to_string(),
            pre_balance: BigDecimal::from(10),
            post_balance: BigDecimal::from(9),
            delta: BigDecimal::from(-1),
            is_fee_payer: true,
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn token_transfer_insert_single() {
        let tt = make_token_transfer();
        let (query, _) = build_token_transfer_insert(&[tt]).unwrap();

        assert!(query.starts_with("INSERT INTO token_transfers"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"));
        assert!(query.contains("ON CONFLICT"));
    }

    #[test]
    fn token_transfer_insert_multiple() {
        let transfers: Vec<_> = (0..3).map(|_| make_token_transfer()).collect();
        let (query, _) = build_token_transfer_insert(&transfers).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"));
        assert!(query.contains("($13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24)"));
        assert!(query.contains("($25, $26, $27, $28, $29, $30, $31, $32, $33, $34, $35, $36)"));
    }

    #[test]
    fn token_transfer_insert_param_count() {
        let transfers: Vec<_> = (0..5).map(|_| make_token_transfer()).collect();
        let (query, _) = build_token_transfer_insert(&transfers).unwrap();
        // 5 rows * 12 params = 60 => highest param is $60
        assert!(query.contains("$60"));
        assert!(!query.contains("$61"));
    }

    // -- native_balance_delta batch insert (P3-W2) --

    #[test]
    fn native_balance_delta_insert_single() {
        let nbd = make_native_balance_delta();
        let (query, _) = build_native_balance_delta_insert(&[nbd]).unwrap();

        assert!(query.starts_with("INSERT INTO native_balance_deltas"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
        assert!(query.contains("ON CONFLICT"));
    }

    #[test]
    fn native_balance_delta_insert_multiple() {
        let deltas: Vec<_> = (0..3).map(|_| make_native_balance_delta()).collect();
        let (query, _) = build_native_balance_delta_insert(&deltas).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
        assert!(query.contains("($12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)"));
    }

    #[test]
    fn native_balance_delta_insert_param_count() {
        let deltas: Vec<_> = (0..5).map(|_| make_native_balance_delta()).collect();
        let (query, _) = build_native_balance_delta_insert(&deltas).unwrap();
        // 5 rows * 11 params = 55 => highest param is $55
        assert!(query.contains("$55"));
        assert!(!query.contains("$56"));
    }

    // -- Repository method signatures (P3-W2) --

    #[test]
    fn repo_save_token_transfers_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.save_token_transfers(&[]));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_token_transfers_by_address_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_token_transfers_by_address("0x1", 10, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_save_native_balance_deltas_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.save_native_balance_deltas(&[]));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_native_balance_deltas_by_account_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_native_balance_deltas_by_account("addr", 10, 0));
        }
        let _ = _check;
    }

    // -- decoded_event batch insert (P3-W3) --

    fn make_decoded_event() -> DecodedEvent {
        DecodedEvent {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "ethereum-mainnet".to_string(),
            program_or_contract: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
            event_signature: Some(
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".to_string(),
            ),
            event_name: Some("Transfer".to_string()),
            log_index: 0,
            decoded_fields: serde_json::json!({"from": "0x1", "to": "0x2", "value": "100"}),
            raw_fields: serde_json::json!({"topics": [], "data": "0x"}),
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn decoded_event_insert_single() {
        let de = make_decoded_event();
        let (query, _) = build_decoded_event_insert(&[de]).unwrap();

        assert!(query.starts_with("INSERT INTO decoded_events"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
        assert!(query.contains("ON CONFLICT"));
        assert!(query.contains("program_or_contract"));
        assert!(query.contains("log_index"));
    }

    #[test]
    fn decoded_event_insert_multiple() {
        let events: Vec<_> = (0..3).map(|_| make_decoded_event()).collect();
        let (query, _) = build_decoded_event_insert(&events).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
        assert!(query.contains("($12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)"));
        assert!(query.contains("($23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33)"));
    }

    #[test]
    fn decoded_event_insert_param_count() {
        let events: Vec<_> = (0..5).map(|_| make_decoded_event()).collect();
        let (query, _) = build_decoded_event_insert(&events).unwrap();
        // 5 rows * 11 params = 55 => highest param is $55
        assert!(query.contains("$55"));
        assert!(!query.contains("$56"));
    }

    #[test]
    fn decoded_event_insert_dedup_clause() {
        let de = make_decoded_event();
        let (query, _) = build_decoded_event_insert(&[de]).unwrap();

        assert!(query.contains("ON CONFLICT (raw_transaction_id, program_or_contract, log_index)"));
        assert!(query.contains("WHERE raw_transaction_id IS NOT NULL"));
        assert!(query.contains("DO NOTHING"));
    }

    // -- Repository method signatures (P3-W3) --

    #[test]
    fn repo_save_decoded_events_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.save_decoded_events(&[]));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_decoded_events_by_contract_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_decoded_events_by_contract("0xabc", 10, 0));
        }
        let _ = _check;
    }

    // -- hl_fill_record batch insert (P3-W4) --

    fn make_hl_fill_record() -> HlFillRecord {
        use bigdecimal::BigDecimal;
        HlFillRecord {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "hypercore-mainnet".to_string(),
            coin: "ETH".to_string(),
            side: "B".to_string(),
            price: BigDecimal::from(3500),
            size: BigDecimal::from(2),
            direction: Some("Open Long".to_string()),
            closed_pnl: Some(BigDecimal::from(0)),
            fee: Some(BigDecimal::from(3)),
            fee_token: Some("USDC".to_string()),
            fill_time: 1700000000000,
            order_id: Some(12345),
            trade_id: Some(67890),
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    fn make_hl_funding_payment() -> HlFundingPayment {
        use bigdecimal::BigDecimal;
        HlFundingPayment {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "hypercore-mainnet".to_string(),
            coin: "ETH".to_string(),
            amount: BigDecimal::from(-3),
            funding_rate: Some(BigDecimal::from(1)),
            payment_time: 1700000000000,
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    fn make_hl_position_change() -> HlPositionChange {
        use bigdecimal::BigDecimal;
        HlPositionChange {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "hypercore-mainnet".to_string(),
            coin: "ETH".to_string(),
            side: "B".to_string(),
            size_delta: BigDecimal::from(2),
            price: BigDecimal::from(3500),
            direction: Some("Open Long".to_string()),
            source_event: "fill".to_string(),
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn hl_fill_record_insert_single() {
        let fill = make_hl_fill_record();
        let (query, _) = build_hl_fill_record_insert(&[fill]).unwrap();

        assert!(query.starts_with("INSERT INTO hl_fill_records"));
        assert!(query
            .contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)"));
        assert!(query.contains("ON CONFLICT"));
    }

    #[test]
    fn hl_fill_record_insert_multiple() {
        let fills: Vec<_> = (0..3).map(|_| make_hl_fill_record()).collect();
        let (query, _) = build_hl_fill_record_insert(&fills).unwrap();

        assert!(query
            .contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)"));
        assert!(query.contains(
            "($17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30, $31, $32)"
        ));
    }

    #[test]
    fn hl_fill_record_insert_param_count() {
        let fills: Vec<_> = (0..5).map(|_| make_hl_fill_record()).collect();
        let (query, _) = build_hl_fill_record_insert(&fills).unwrap();
        // 5 rows * 16 params = 80 => highest param is $80
        assert!(query.contains("$80"));
        assert!(!query.contains("$81"));
    }

    #[test]
    fn hl_fill_record_insert_dedup_clause() {
        let fill = make_hl_fill_record();
        let (query, _) = build_hl_fill_record_insert(&[fill]).unwrap();

        assert!(query.contains("ON CONFLICT (raw_transaction_id, coin, fill_time, side)"));
        assert!(query.contains("WHERE raw_transaction_id IS NOT NULL"));
        assert!(query.contains("DO NOTHING"));
    }

    // -- hl_funding_payment batch insert (P3-W4) --

    #[test]
    fn hl_funding_payment_insert_single() {
        let payment = make_hl_funding_payment();
        let (query, _) = build_hl_funding_payment_insert(&[payment]).unwrap();

        assert!(query.starts_with("INSERT INTO hl_funding_payments"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        assert!(query.contains("ON CONFLICT"));
    }

    #[test]
    fn hl_funding_payment_insert_multiple() {
        let payments: Vec<_> = (0..3).map(|_| make_hl_funding_payment()).collect();
        let (query, _) = build_hl_funding_payment_insert(&payments).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        assert!(query.contains("($10, $11, $12, $13, $14, $15, $16, $17, $18)"));
        assert!(query.contains("($19, $20, $21, $22, $23, $24, $25, $26, $27)"));
    }

    #[test]
    fn hl_funding_payment_insert_param_count() {
        let payments: Vec<_> = (0..5).map(|_| make_hl_funding_payment()).collect();
        let (query, _) = build_hl_funding_payment_insert(&payments).unwrap();
        // 5 rows * 9 params = 45 => highest param is $45
        assert!(query.contains("$45"));
        assert!(!query.contains("$46"));
    }

    #[test]
    fn hl_funding_payment_insert_dedup_clause() {
        let payment = make_hl_funding_payment();
        let (query, _) = build_hl_funding_payment_insert(&[payment]).unwrap();

        assert!(query.contains("ON CONFLICT (raw_transaction_id, coin, payment_time)"));
        assert!(query.contains("WHERE raw_transaction_id IS NOT NULL"));
        assert!(query.contains("DO NOTHING"));
    }

    // -- hl_position_change batch insert (P3-W4) --

    #[test]
    fn hl_position_change_insert_single() {
        let change = make_hl_position_change();
        let (query, _) = build_hl_position_change_insert(&[change]).unwrap();

        assert!(query.starts_with("INSERT INTO hl_position_changes"));
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
        assert!(query.contains("ON CONFLICT"));
    }

    #[test]
    fn hl_position_change_insert_multiple() {
        let changes: Vec<_> = (0..3).map(|_| make_hl_position_change()).collect();
        let (query, _) = build_hl_position_change_insert(&changes).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
        assert!(query.contains("($12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)"));
        assert!(query.contains("($23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33)"));
    }

    #[test]
    fn hl_position_change_insert_param_count() {
        let changes: Vec<_> = (0..5).map(|_| make_hl_position_change()).collect();
        let (query, _) = build_hl_position_change_insert(&changes).unwrap();
        // 5 rows * 11 params = 55 => highest param is $55
        assert!(query.contains("$55"));
        assert!(!query.contains("$56"));
    }

    #[test]
    fn hl_position_change_insert_dedup_clause() {
        let change = make_hl_position_change();
        let (query, _) = build_hl_position_change_insert(&[change]).unwrap();

        assert!(query.contains("ON CONFLICT (raw_transaction_id, coin, side, source_event)"));
        assert!(query.contains("WHERE raw_transaction_id IS NOT NULL"));
        assert!(query.contains("DO NOTHING"));
    }

    // -- Repository method signatures (P3-W4) --

    #[test]
    fn repo_save_hl_fill_records_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.save_hl_fill_records(&[]));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_hl_fill_records_by_coin_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_hl_fill_records_by_coin("ETH", 10, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_save_hl_funding_payments_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.save_hl_funding_payments(&[]));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_hl_funding_payments_by_coin_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_hl_funding_payments_by_coin("ETH", 10, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_save_hl_position_changes_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.save_hl_position_changes(&[]));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_hl_position_changes_by_coin_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_hl_position_changes_by_coin("ETH", 10, 0));
        }
        let _ = _check;
    }

    // -- completeness_status SQL helpers (P3-W6) --

    #[test]
    fn completeness_status_sql_roundtrip() {
        for status in [
            CompletenessStatus::Partial,
            CompletenessStatus::Complete,
            CompletenessStatus::Backfilling,
            CompletenessStatus::Gap,
        ] {
            let s = completeness_status_to_sql(&status);
            let back = sql_to_completeness_status(s).unwrap();
            assert_eq!(status, back, "roundtrip failed for {status:?}");
        }
    }

    #[test]
    fn completeness_status_sql_unknown() {
        assert!(sql_to_completeness_status("missing").is_err());
    }

    // -- dataset_completeness upsert (P3-W6) --

    fn make_dataset_completeness() -> DatasetCompleteness {
        let now = Utc::now();
        DatasetCompleteness {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            dataset_name: "token_transfers".to_string(),
            dataset_version_id: None,
            network: "solana-mainnet".to_string(),
            status: CompletenessStatus::Partial,
            coverage_start: Some(1700000000),
            coverage_end: Some(1700100000),
            block_start: Some(200_000_000),
            block_end: Some(200_100_000),
            last_ingestion_run_id: None,
            records_count: 42,
            gap_ranges: None,
            notes: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn dataset_completeness_upsert_query_structure() {
        let dc = make_dataset_completeness();
        let (query, _) = build_dataset_completeness_upsert(&dc).unwrap();

        assert!(query.starts_with("INSERT INTO dataset_completeness"));
        assert!(query.contains("ON CONFLICT (target_id, dataset_name, network)"));
        assert!(query.contains("DO UPDATE SET"));
        assert!(query.contains("status = EXCLUDED.status"));
        assert!(query.contains("coverage_start = EXCLUDED.coverage_start"));
        assert!(query.contains("coverage_end = EXCLUDED.coverage_end"));
        assert!(query.contains("records_count = EXCLUDED.records_count"));
        assert!(query.contains("updated_at = EXCLUDED.updated_at"));
    }

    #[test]
    fn dataset_completeness_upsert_has_16_params() {
        let dc = make_dataset_completeness();
        let (query, _) = build_dataset_completeness_upsert(&dc).unwrap();
        assert!(query.contains("$16"));
        assert!(!query.contains("$17"));
    }

    #[test]
    fn dataset_completeness_upsert_idempotent_query() {
        let dc = make_dataset_completeness();
        let (query1, _) = build_dataset_completeness_upsert(&dc).unwrap();
        let (query2, _) = build_dataset_completeness_upsert(&dc).unwrap();
        assert_eq!(query1, query2, "upsert query should be deterministic");
    }

    // -- Repository method signatures (P3-W6) --

    #[test]
    fn repo_upsert_dataset_completeness_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            let dc = DatasetCompleteness {
                id: Uuid::new_v4(),
                target_id: Uuid::new_v4(),
                dataset_name: "test".to_string(),
                dataset_version_id: None,
                network: "test-net".to_string(),
                status: CompletenessStatus::Partial,
                coverage_start: None,
                coverage_end: None,
                block_start: None,
                block_end: None,
                last_ingestion_run_id: None,
                records_count: 0,
                gap_ranges: None,
                notes: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            _assert_send(repo.upsert_dataset_completeness(&dc));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_dataset_completeness_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_dataset_completeness(Uuid::new_v4(), "test", "net"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_list_completeness_by_target_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.list_completeness_by_target(Uuid::new_v4()));
        }
        let _ = _check;
    }

    #[test]
    fn repo_list_completeness_by_dataset_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.list_completeness_by_dataset("token_transfers"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_list_completeness_by_status_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.list_completeness_by_status(CompletenessStatus::Partial));
        }
        let _ = _check;
    }

    #[test]
    fn repo_update_completeness_after_run_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.update_completeness_after_run(
                Uuid::new_v4(),
                "token_transfers",
                "solana-mainnet",
                CompletenessStatus::Complete,
                Some(1700000000),
                Some(1700100000),
                Some(200_000_000),
                Some(200_100_000),
                100,
                Uuid::new_v4(),
            ));
        }
        let _ = _check;
    }

    // -- Dataset filter query builder (P4-W1) --

    #[test]
    fn dataset_filter_query_no_filters() {
        let (sql, _) = build_dataset_filter_query(
            "dt.id, dt.network",
            DatasetName::TokenTransfers.physical_table(),
            "dt.created_at",
            None,
            None,
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.starts_with("SELECT dt.id, dt.network FROM token_transfers dt"));
        assert!(!sql.contains("JOIN"));
        assert!(!sql.contains("WHERE"));
        assert!(sql.contains("ORDER BY dt.created_at DESC"));
        assert!(sql.contains("LIMIT $1 OFFSET $2"));
    }

    #[test]
    fn dataset_filter_query_target_only() {
        let tid = Uuid::new_v4();
        let (sql, _) = build_dataset_filter_query(
            "dt.id",
            DatasetName::TokenTransfers.physical_table(),
            "dt.created_at",
            Some(tid),
            None,
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("JOIN target_matches tm ON"));
        assert!(!sql.contains("JOIN raw_transactions"));
        assert!(sql.contains("WHERE tm.target_id = $1"));
        assert!(sql.contains("LIMIT $2 OFFSET $3"));
    }

    #[test]
    fn dataset_filter_query_network_only() {
        let (sql, _) = build_dataset_filter_query(
            "dt.id",
            DatasetName::TokenTransfers.physical_table(),
            "dt.created_at",
            None,
            Some("solana-mainnet"),
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(!sql.contains("JOIN"));
        assert!(sql.contains("WHERE dt.network = $1"));
        assert!(sql.contains("LIMIT $2 OFFSET $3"));
    }

    #[test]
    fn dataset_filter_query_time_window_only() {
        let (sql, _) = build_dataset_filter_query(
            "dt.id",
            DatasetName::TokenTransfers.physical_table(),
            "dt.created_at",
            None,
            None,
            Some(1700000000),
            Some(1700100000),
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("JOIN raw_transactions rt ON"));
        assert!(sql.contains("rt.timestamp >= $1"));
        assert!(sql.contains("rt.timestamp <= $2"));
        assert!(sql.contains("LIMIT $3 OFFSET $4"));
    }

    #[test]
    fn dataset_filter_query_all_filters() {
        let tid = Uuid::new_v4();
        let (sql, _) = build_dataset_filter_query(
            "dt.id",
            DatasetName::TokenTransfers.physical_table(),
            "dt.created_at",
            Some(tid),
            Some("solana-mainnet"),
            Some(1700000000),
            Some(1700100000),
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("JOIN target_matches tm ON"));
        assert!(sql.contains("JOIN raw_transactions rt ON"));
        assert!(sql.contains("tm.target_id = $1"));
        assert!(sql.contains("dt.network = $2"));
        assert!(sql.contains("rt.timestamp >= $3"));
        assert!(sql.contains("rt.timestamp <= $4"));
        assert!(sql.contains("LIMIT $5 OFFSET $6"));
    }

    #[test]
    fn dataset_filter_query_order_col_customizable() {
        let (sql, _) = build_dataset_filter_query(
            "dt.id",
            DatasetName::HlFills.physical_table(),
            "dt.fill_time",
            None,
            None,
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("ORDER BY dt.fill_time DESC"));
    }

    // -- Dataset query method signatures (P4-W1) --

    #[test]
    fn repo_query_token_transfers_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_token_transfers(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_query_native_balance_deltas_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_native_balance_deltas(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_query_decoded_events_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_decoded_events(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_query_hl_fill_records_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_hl_fill_records(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_query_hl_funding_payments_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_hl_funding_payments(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_query_hl_position_changes_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_hl_position_changes(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    // -- P5-W1: wallet_ledger and balance_history query builders --

    #[test]
    fn wallet_ledger_query_builder_basic() {
        let (sql, _) = build_dataset_filter_query(
            "dt.*",
            DatasetName::WalletLedger.physical_table(),
            "dt.timestamp",
            None,
            None,
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("wallet_ledger"));
        assert!(sql.contains("ORDER BY dt.timestamp DESC"));
    }

    #[test]
    fn wallet_ledger_query_builder_with_target() {
        let tid = Uuid::new_v4();
        let (sql, _) = build_dataset_filter_query(
            "dt.*",
            DatasetName::WalletLedger.physical_table(),
            "dt.timestamp",
            Some(tid),
            None,
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("JOIN target_matches"));
        assert!(sql.contains("tm.target_id"));
    }

    #[test]
    fn balance_history_query_builder_basic() {
        let (sql, _) = build_dataset_filter_query(
            "dt.*",
            DatasetName::BalanceHistory.physical_table(),
            "dt.timestamp",
            None,
            None,
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("balance_history"));
    }

    #[test]
    fn balance_history_query_builder_with_network() {
        let (sql, _) = build_dataset_filter_query(
            "dt.*",
            DatasetName::BalanceHistory.physical_table(),
            "dt.timestamp",
            None,
            Some("solana-mainnet"),
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("dt.network"));
    }

    #[test]
    fn repo_query_wallet_ledger_records_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_wallet_ledger_records(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_query_balance_snapshots_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_balance_snapshots(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    // -- P5-W3: protocol_events and pool_snapshots query builders --

    #[test]
    fn protocol_events_query_builder_basic() {
        let (sql, _) = build_dataset_filter_query(
            "dt.*",
            DatasetName::ProtocolEvents.physical_table(),
            "dt.timestamp",
            None,
            None,
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("protocol_events"));
        assert!(sql.contains("LIMIT"));
    }

    #[test]
    fn protocol_events_query_builder_with_network() {
        let (sql, _) = build_dataset_filter_query(
            "dt.*",
            DatasetName::ProtocolEvents.physical_table(),
            "dt.timestamp",
            None,
            Some("ethereum-mainnet"),
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("dt.network"));
    }

    #[test]
    fn pool_snapshots_query_builder_basic() {
        let (sql, _) = build_dataset_filter_query(
            "dt.*",
            DatasetName::PoolSnapshots.physical_table(),
            "dt.snapshot_timestamp",
            None,
            None,
            None,
            None,
            50,
            0,
        )
        .unwrap();
        assert!(sql.contains("pool_snapshots"));
        assert!(sql.contains("LIMIT"));
    }

    #[test]
    fn pool_snapshots_query_builder_with_time_range() {
        let (sql, _) = build_dataset_filter_query(
            "dt.*",
            DatasetName::PoolSnapshots.physical_table(),
            "dt.snapshot_timestamp",
            None,
            None,
            Some(1000),
            Some(2000),
            50,
            0,
        )
        .unwrap();
        // Time filtering joins raw_transactions and uses rt.timestamp
        assert!(sql.contains("rt.timestamp >="));
        assert!(sql.contains("rt.timestamp <="));
    }

    #[test]
    fn repo_query_protocol_events_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_protocol_events(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_query_pool_snapshots_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.query_pool_snapshots(None, None, None, None, 50, 0));
        }
        let _ = _check;
    }

    // -- raw_evm_trace batch insert --

    fn make_raw_evm_trace() -> RawEvmTrace {
        RawEvmTrace {
            id: Uuid::new_v4(),
            transaction_hash: "0xdeadbeef1234567890".to_string(),
            block_number: Some(18_000_000),
            network: "ethereum-mainnet".to_string(),
            trace_type: EvmTraceType::CallTracer,
            raw_trace: serde_json::json!({
                "type": "CALL",
                "from": "0x1111111111111111111111111111111111111111",
                "to": "0x2222222222222222222222222222222222222222",
                "value": "0xde0b6b3a7640000",
                "gas": "0x5208",
                "gasUsed": "0x5208",
                "input": "0x",
                "output": "0x",
            }),
            ingestion_run_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn raw_evm_trace_insert_has_8_params_per_row() {
        let trace = make_raw_evm_trace();
        let (query, _) = build_raw_evm_trace_insert(&[trace]).unwrap();

        assert!(query.starts_with("INSERT INTO raw_evm_traces"));
        assert!(query.contains("$8"));
        assert!(!query.contains("$9"));
        assert!(query.contains("ON CONFLICT (network, transaction_hash, trace_type) DO NOTHING"));
    }

    #[test]
    fn raw_evm_trace_insert_multiple_rows() {
        let traces = vec![make_raw_evm_trace(), make_raw_evm_trace()];
        let (query, _) = build_raw_evm_trace_insert(&traces).unwrap();

        assert!(query.contains("$16"));
        assert!(!query.contains("$17"));
    }

    #[test]
    fn raw_evm_trace_insert_preserves_trace_type_string() {
        let mut trace = make_raw_evm_trace();
        trace.trace_type = EvmTraceType::PrestateTracer;
        let (query, _) = build_raw_evm_trace_insert(&[trace]).unwrap();
        assert!(query.contains("raw_evm_traces"));
    }

    #[test]
    fn raw_evm_trace_insert_empty_batch() {
        // Empty batch produces syntactically incomplete SQL (no VALUES rows).
        // This is a known pre-existing pattern: callers (save_raw_evm_traces)
        // guard against it via .chunks() which yields no chunks for empty input.
        let traces: Vec<RawEvmTrace> = vec![];
        let result = build_raw_evm_trace_insert(&traces);
        assert!(result.is_ok());
    }

    // -- raw_evm_trace repo methods are Send --

    #[test]
    fn repo_save_raw_evm_traces_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.save_raw_evm_traces(&[]));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_raw_evm_trace_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_raw_evm_trace(
                "ethereum-mainnet",
                "0xdeadbeef",
                EvmTraceType::CallTracer,
            ));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_raw_evm_traces_by_block_range_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_raw_evm_traces_by_block_range("ethereum-mainnet", 100, 200));
        }
        let _ = _check;
    }

    // -- Durable Control Plane row mapper tests --

    #[test]
    fn row_to_ingestion_job_parses_all_statuses() {
        // This test verifies that IngestionJobStatus::from_str covers all
        // valid statuses that the CHECK constraint allows.
        use std::str::FromStr;
        for s in [
            "pending",
            "claimed",
            "running",
            "completed",
            "failed",
            "cancelled",
        ] {
            let result = IngestionJobStatus::from_str(s);
            assert!(
                result.is_ok(),
                "IngestionJobStatus::from_str('{s}') should succeed"
            );
        }
        assert!(IngestionJobStatus::from_str("invalid").is_err());
    }

    #[test]
    fn row_to_ingestion_job_parses_all_modes() {
        use std::str::FromStr;
        for s in ["backfill", "incremental"] {
            let result = IngestionJobMode::from_str(s);
            assert!(
                result.is_ok(),
                "IngestionJobMode::from_str('{s}') should succeed"
            );
        }
        assert!(IngestionJobMode::from_str("invalid").is_err());
    }

    #[test]
    fn stream_source_from_str_covers_all_variants() {
        use std::str::FromStr;
        for s in ["grpc", "ws", "rpc"] {
            let result = StreamSource::from_str(s);
            assert!(
                result.is_ok(),
                "StreamSource::from_str('{s}') should succeed"
            );
        }
        assert!(StreamSource::from_str("invalid").is_err());
    }

    #[test]
    fn stream_desired_status_from_str_covers_all_variants() {
        use std::str::FromStr;
        for s in ["active", "paused", "stopped"] {
            let result = StreamDesiredStatus::from_str(s);
            assert!(
                result.is_ok(),
                "StreamDesiredStatus::from_str('{s}') should succeed"
            );
        }
        assert!(StreamDesiredStatus::from_str("invalid").is_err());
    }

    #[test]
    fn stream_actual_status_from_str_covers_all_variants() {
        use std::str::FromStr;
        for s in ["pending", "running", "paused", "stopped", "error"] {
            let result = StreamActualStatus::from_str(s);
            assert!(
                result.is_ok(),
                "StreamActualStatus::from_str('{s}') should succeed"
            );
        }
        assert!(StreamActualStatus::from_str("invalid").is_err());
    }

    #[test]
    fn export_job_status_from_str_covers_all_variants() {
        use std::str::FromStr;
        for s in ["pending", "running", "delivering", "completed", "failed"] {
            let result = ExportJobStatus::from_str(s);
            assert!(
                result.is_ok(),
                "ExportJobStatus::from_str('{s}') should succeed"
            );
        }
        assert!(ExportJobStatus::from_str("invalid").is_err());
    }

    #[test]
    fn materialization_run_status_from_str_covers_all_variants() {
        use std::str::FromStr;
        for s in ["pending", "running", "completed", "failed"] {
            let result = MaterializationRunStatus::from_str(s);
            assert!(
                result.is_ok(),
                "MaterializationRunStatus::from_str('{s}') should succeed"
            );
        }
        assert!(MaterializationRunStatus::from_str("invalid").is_err());
    }

    // -- Durable Control Plane repo methods are Send --

    #[test]
    fn repo_enqueue_ingestion_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            let params = EnqueueIngestionJobParams {
                target_id: None,
                network: "solana-mainnet",
                mode: "backfill",
                priority: 0,
                idempotency_key: None,
                requested_by: None,
                callback_url: None,
            };
            _assert_send(repo.enqueue_ingestion_job(&params));
        }
        let _ = _check;
    }

    #[test]
    fn repo_claim_ingestion_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.claim_ingestion_job("worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_heartbeat_ingestion_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.heartbeat_ingestion_job(Uuid::new_v4(), "worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_complete_ingestion_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.complete_ingestion_job(Uuid::new_v4(), "worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_fail_ingestion_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.fail_ingestion_job(Uuid::new_v4(), "worker-1", "boom"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_ingestion_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_ingestion_job(Uuid::new_v4()));
        }
        let _ = _check;
    }

    #[test]
    fn repo_list_ingestion_jobs_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.list_ingestion_jobs(Some("pending"), 10, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_upsert_stream_subscription_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.upsert_stream_subscription(
                None,
                "solana-mainnet",
                "grpc",
                "active",
                None,
            ));
        }
        let _ = _check;
    }

    #[test]
    fn repo_claim_stream_lease_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.claim_stream_lease(Uuid::new_v4(), "worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_heartbeat_stream_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.heartbeat_stream(Uuid::new_v4(), "worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_release_stream_lease_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.release_stream_lease(Uuid::new_v4(), "worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_set_stream_desired_status_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.set_stream_desired_status(Uuid::new_v4(), "stopped"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_fail_stream_subscription_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.fail_stream_subscription(Uuid::new_v4(), "worker-1", "boom"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_list_claimable_stream_subscriptions_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.list_claimable_stream_subscriptions(10));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_stream_subscription_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_stream_subscription(Uuid::new_v4()));
        }
        let _ = _check;
    }

    #[test]
    fn repo_list_stream_subscriptions_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.list_stream_subscriptions(Some("active"), None, 10, 0));
        }
        let _ = _check;
    }

    #[test]
    fn repo_enqueue_export_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.enqueue_export_job(
                DatasetName::TokenTransfers,
                ExportFormat::Csv,
                None,
                None,
                None,
            ));
        }
        let _ = _check;
    }

    #[test]
    fn repo_claim_export_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.claim_export_job("worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_update_export_job_status_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.update_export_job_status(
                Uuid::new_v4(),
                "completed",
                Some(42),
                Some("/tmp/out.csv"),
                None,
                "worker-1",
                None,
                None,
                None,
                None,
                None,
                None,
            ));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_export_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_export_job(Uuid::new_v4()));
        }
        let _ = _check;
    }

    #[test]
    fn repo_create_materialization_run_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.create_materialization_run(
                "token_transfers",
                None,
                None,
                None,
                None,
            ));
        }
        let _ = _check;
    }

    #[test]
    fn repo_complete_materialization_run_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.complete_materialization_run(Uuid::new_v4(), "worker-1", 100));
        }
        let _ = _check;
    }

    #[test]
    fn repo_fail_materialization_run_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.fail_materialization_run(Uuid::new_v4(), "worker-1", "parse error"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_claim_materialization_run_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.claim_materialization_run(Uuid::new_v4(), "worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_heartbeat_materialization_run_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.heartbeat_materialization_run(Uuid::new_v4(), "worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn repo_get_materialization_run_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.get_materialization_run(Uuid::new_v4()));
        }
        let _ = _check;
    }

    #[test]
    fn repo_list_claimable_materialization_runs_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.list_claimable_materialization_runs(5));
        }
        let _ = _check;
    }

    #[test]
    fn repo_update_stream_cursor_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            let cursor = serde_json::json!({"last_slot": 300});
            _assert_send(repo.update_stream_cursor(Uuid::new_v4(), &cursor));
        }
        let _ = _check;
    }

    #[test]
    fn repo_heartbeat_export_job_is_send() {
        fn _assert_send<F: std::future::Future + Send>(_: F) {}
        fn _check(repo: &Repository) {
            _assert_send(repo.heartbeat_export_job(Uuid::new_v4(), "worker-1"));
        }
        let _ = _check;
    }

    #[test]
    fn stale_job_threshold_is_positive() {
        const { assert!(Repository::STALE_JOB_THRESHOLD_MINUTES > 0) };
    }

    #[test]
    fn export_job_has_heartbeat_at_field() {
        // Verify the ExportJob struct includes heartbeat_at, preventing
        // accidental removal that would break dead-worker recovery.
        let job = ExportJob {
            id: Uuid::new_v4(),
            dataset: DatasetName::TokenTransfers,
            format: ExportFormat::Jsonl,
            filters: None,
            sink_config: None,
            status: ExportJobStatus::Pending,
            worker_id: None,
            record_count: None,
            result_location: None,
            delivery_destination: None,
            error_message: None,
            dataset_version_id: None,
            dataset_version: None,
            completeness_status: None,
            completeness_coverage: None,
            last_ingestion_run_id: None,
            owner_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            started_at: None,
            completed_at: None,
            heartbeat_at: None,
        };
        assert!(job.heartbeat_at.is_none());
    }
}
