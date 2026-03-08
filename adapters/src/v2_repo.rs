//! V2 repository methods for canonical bronze data and control-plane tables.
//!
//! All methods are added as `impl Repository` so callers use the same
//! `Repository` value they already hold for V1 wallet-scoped queries.

use chrono::{DateTime, Utc};
use spectraplex_core::materializer::{NativeBalanceDelta, TokenTransfer};
use spectraplex_core::v2::{
    ChainFamily, Checkpoint, DatasetVersion, DatasetVersionStatus, IndexTarget, IngestionRun,
    Network, RawTransaction, TargetKind, TargetMatch, TargetMode,
};
use sqlx::Row;
use uuid::Uuid;

use crate::repo::Repository;

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
    Ok(IngestionRun {
        id: row.try_get("id")?,
        target_id: row.try_get("target_id")?,
        network: row.try_get("network")?,
        source: row.try_get("source")?,
        mode: row.try_get("mode")?,
        status: row.try_get("status")?,
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

/// Build the INSERT for a single `index_target` with ON CONFLICT DO NOTHING.
pub fn build_index_target_insert(
    t: &IndexTarget,
) -> anyhow::Result<(String, sqlx::postgres::PgArguments)> {
    let query = String::from(
        "INSERT INTO index_targets \
         (id, kind, network, chain_family, address, filter_spec, mode, label, owner_id, created_at, updated_at) \
         VALUES ($1, $2::target_kind_enum, $3, $4::chain_family_enum, $5, $6, $7::target_mode_enum, $8, $9, $10, $11)",
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
    args.add(&run.mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    args.add(&run.status).map_err(|e| anyhow::anyhow!("{e}"))?;
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
         (id, raw_transaction_id, network, token_address, token_symbol, from_address, to_address, amount, decimals, dataset_version_id, created_at) \
         VALUES ",
    );
    let mut args = sqlx::postgres::PgArguments::default();
    for (i, t) in transfers.iter().enumerate() {
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
        args.add(t.dataset_version_id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        args.add(t.created_at).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    query.push_str(
        " ON CONFLICT (raw_transaction_id, from_address, to_address, token_address, amount) \
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
        " ON CONFLICT (raw_transaction_id, account_address) \
         WHERE raw_transaction_id IS NOT NULL DO NOTHING",
    );
    Ok((query, args))
}

// ---------------------------------------------------------------------------
// V2 Repository impl
// ---------------------------------------------------------------------------

impl Repository {
    /// Batch size for V2 chunked inserts.
    const V2_BATCH_SIZE: usize = 500;

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
    ) -> anyhow::Result<Option<IndexTarget>> {
        let row = sqlx::query(
            "SELECT id, kind::text, network, chain_family::text, address, filter_spec, \
             mode::text, label, owner_id, created_at, updated_at \
             FROM index_targets \
             WHERE kind = $1::target_kind_enum AND network = $2 AND address = $3",
        )
        .bind(target_kind_to_sql(&kind))
        .bind(network)
        .bind(address)
        .fetch_optional(self.pool())
        .await?;
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
             from_address, to_address, amount, decimals, dataset_version_id, created_at \
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
             from_address, to_address, amount, decimals, dataset_version_id, created_at \
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
            mode: "backfill".to_string(),
            status: "running".to_string(),
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
        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
        assert!(query.contains("ON CONFLICT"));
    }

    #[test]
    fn token_transfer_insert_multiple() {
        let transfers: Vec<_> = (0..3).map(|_| make_token_transfer()).collect();
        let (query, _) = build_token_transfer_insert(&transfers).unwrap();

        assert!(query.contains("($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"));
        assert!(query.contains("($12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22)"));
        assert!(query.contains("($23, $24, $25, $26, $27, $28, $29, $30, $31, $32, $33)"));
    }

    #[test]
    fn token_transfer_insert_param_count() {
        let transfers: Vec<_> = (0..5).map(|_| make_token_transfer()).collect();
        let (query, _) = build_token_transfer_insert(&transfers).unwrap();
        // 5 rows * 11 params = 55 => highest param is $55
        assert!(query.contains("$55"));
        assert!(!query.contains("$56"));
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
}
