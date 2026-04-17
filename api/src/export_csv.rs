//! Streaming-friendly CSV row writers for export datasets.
//!
//! Each dataset has two helpers:
//!
//! - `*_csv_header()` — returns the header line (including the trailing
//!   newline). Emit once per file for CSV exports.
//! - `write_*_csv_rows(records, w)` — appends row lines to any
//!   [`std::io::Write`]. Used by both the streaming export writer (which
//!   writes directly into a file's `BufWriter`) and by the legacy
//!   `*_to_csv` helpers in `main.rs` (which collect into a `Vec<u8>` for
//!   the in-memory handler paths such as tax export).
//!
//! Keeping the row emission logic separate from the `Vec<u8>`-collecting
//! convenience wrappers is what lets the export worker stop buffering the
//! entire dataset in memory (#208).
//!
//! Output format is kept byte-identical to the previous `*_to_csv`
//! implementations, so the existing `*_to_csv_format` tests still pass.

use bigdecimal::BigDecimal;
use spectraplex_core::materializer::{
    BalanceSnapshot, DecodedEvent, HlFillRecord, HlFundingPayment, HlPnlSummary, HlPositionChange,
    HlTradeHistory, NativeBalanceDelta, PoolSnapshot, ProtocolEvent, TokenTransfer,
    WalletLedgerRecord,
};
use std::io::Write;

/// Quote a CSV field if it contains any of `,`, `"`, or newline.
pub(crate) fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// token_transfers
// ---------------------------------------------------------------------------

pub(crate) fn token_transfers_csv_header() -> &'static str {
    "id,raw_transaction_id,network,token_address,token_symbol,from_address,to_address,amount,decimals,transfer_index,dataset_version_id,created_at\n"
}

pub(crate) fn write_token_transfers_csv_rows<W: Write>(
    records: &[TokenTransfer],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.token_address),
            r.token_symbol.as_deref().unwrap_or(""),
            csv_escape(&r.from_address),
            csv_escape(&r.to_address),
            r.amount,
            r.decimals,
            r.transfer_index,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// native_balance_deltas
// ---------------------------------------------------------------------------

pub(crate) fn native_balance_deltas_csv_header() -> &'static str {
    "id,raw_transaction_id,network,account_address,native_token,pre_balance,post_balance,delta,is_fee_payer,dataset_version_id,created_at\n"
}

pub(crate) fn write_native_balance_deltas_csv_rows<W: Write>(
    records: &[NativeBalanceDelta],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.account_address),
            csv_escape(&r.native_token),
            r.pre_balance,
            r.post_balance,
            r.delta,
            r.is_fee_payer,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// decoded_events
// ---------------------------------------------------------------------------

pub(crate) fn decoded_events_csv_header() -> &'static str {
    "id,raw_transaction_id,network,program_or_contract,event_signature,event_name,log_index,dataset_version_id,created_at\n"
}

pub(crate) fn write_decoded_events_csv_rows<W: Write>(
    records: &[DecodedEvent],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{}",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.program_or_contract),
            csv_escape(r.event_signature.as_deref().unwrap_or("")),
            csv_escape(r.event_name.as_deref().unwrap_or("")),
            r.log_index,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// hl_fills
// ---------------------------------------------------------------------------

pub(crate) fn hl_fills_csv_header() -> &'static str {
    "id,raw_transaction_id,network,coin,side,price,size,direction,closed_pnl,fee,fee_token,fill_time,order_id,trade_id,dataset_version_id,created_at\n"
}

pub(crate) fn write_hl_fills_csv_rows<W: Write>(
    records: &[HlFillRecord],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.coin),
            csv_escape(&r.side),
            r.price,
            r.size,
            r.direction.as_deref().unwrap_or(""),
            r.closed_pnl
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.fee.as_ref().map(|v| v.to_string()).unwrap_or_default(),
            r.fee_token.as_deref().unwrap_or(""),
            r.fill_time,
            r.order_id.map(|v| v.to_string()).unwrap_or_default(),
            r.trade_id.map(|v| v.to_string()).unwrap_or_default(),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// hl_funding
// ---------------------------------------------------------------------------

pub(crate) fn hl_funding_csv_header() -> &'static str {
    "id,raw_transaction_id,network,coin,amount,funding_rate,payment_time,dataset_version_id,created_at\n"
}

pub(crate) fn write_hl_funding_csv_rows<W: Write>(
    records: &[HlFundingPayment],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{}",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.coin),
            r.amount,
            r.funding_rate
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.payment_time,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// hl_positions
// ---------------------------------------------------------------------------

pub(crate) fn hl_positions_csv_header() -> &'static str {
    "id,raw_transaction_id,network,coin,side,size_delta,price,direction,source_event,dataset_version_id,created_at\n"
}

pub(crate) fn write_hl_positions_csv_rows<W: Write>(
    records: &[HlPositionChange],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.network),
            csv_escape(&r.coin),
            csv_escape(&r.side),
            r.size_delta,
            r.price,
            r.direction.as_deref().unwrap_or(""),
            csv_escape(&r.source_event),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// wallet_ledger
// ---------------------------------------------------------------------------

pub(crate) fn wallet_ledger_csv_header() -> &'static str {
    "id,raw_transaction_id,wallet_address,network,tx_hash,timestamp,entry_type,asset_symbol,amount,counterparty_address,fee_amount,fee_asset,cost_basis,proceeds,dataset_version_id,created_at\n"
}

pub(crate) fn write_wallet_ledger_csv_rows<W: Write>(
    records: &[WalletLedgerRecord],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            r.raw_transaction_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            csv_escape(&r.wallet_address),
            csv_escape(&r.network),
            csv_escape(&r.tx_hash),
            r.timestamp,
            csv_escape(&r.entry_type),
            csv_escape(&r.asset_symbol),
            r.amount,
            r.counterparty_address.as_deref().unwrap_or(""),
            r.fee_amount
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.fee_asset.as_deref().unwrap_or(""),
            r.cost_basis
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.proceeds
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// balance_history
// ---------------------------------------------------------------------------

pub(crate) fn balance_history_csv_header() -> &'static str {
    "id,wallet_address,asset_symbol,network,timestamp,balance,tx_hash,dataset_version_id,created_at\n"
}

pub(crate) fn write_balance_history_csv_rows<W: Write>(
    records: &[BalanceSnapshot],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{}",
            r.id,
            csv_escape(&r.wallet_address),
            csv_escape(&r.asset_symbol),
            csv_escape(&r.network),
            r.timestamp,
            r.balance,
            csv_escape(&r.tx_hash),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// hl_pnl_summary
// ---------------------------------------------------------------------------

pub(crate) fn hl_pnl_summary_csv_header() -> &'static str {
    "id,wallet_address,coin,network,period_start,period_end,total_closed_pnl,total_funding,total_fees,net_pnl,trade_count,fill_count,avg_trade_size,win_count,loss_count,dataset_version_id,created_at\n"
}

pub(crate) fn write_hl_pnl_summary_csv_rows<W: Write>(
    records: &[HlPnlSummary],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            csv_escape(&r.wallet_address),
            csv_escape(&r.coin),
            csv_escape(&r.network),
            r.period_start,
            r.period_end,
            r.total_closed_pnl,
            r.total_funding,
            r.total_fees,
            r.net_pnl,
            r.trade_count,
            r.fill_count,
            r.avg_trade_size,
            r.win_count,
            r.loss_count,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// hl_trade_history
// ---------------------------------------------------------------------------

pub(crate) fn hl_trade_history_csv_header() -> &'static str {
    "id,wallet_address,coin,network,side,entry_price,exit_price,size,opened_at,closed_at,realized_pnl,fees,num_fills,dataset_version_id,created_at\n"
}

pub(crate) fn write_hl_trade_history_csv_rows<W: Write>(
    records: &[HlTradeHistory],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            csv_escape(&r.wallet_address),
            csv_escape(&r.coin),
            csv_escape(&r.network),
            csv_escape(&r.side),
            r.entry_price,
            r.exit_price,
            r.size,
            r.opened_at,
            r.closed_at,
            r.realized_pnl,
            r.fees,
            r.num_fills,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// protocol_events
// ---------------------------------------------------------------------------

pub(crate) fn protocol_events_csv_header() -> &'static str {
    "id,network,protocol_address,protocol_name,event_type,event_details,pool_address,raw_event_id,timestamp,dataset_version_id,created_at\n"
}

pub(crate) fn write_protocol_events_csv_rows<W: Write>(
    records: &[ProtocolEvent],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            csv_escape(&r.network),
            csv_escape(&r.protocol_address),
            r.protocol_name.as_deref().unwrap_or(""),
            csv_escape(&r.event_type),
            csv_escape(&r.event_details.to_string()),
            r.pool_address.as_deref().unwrap_or(""),
            r.raw_event_id.map(|u| u.to_string()).unwrap_or_default(),
            r.timestamp,
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pool_snapshots
// ---------------------------------------------------------------------------

pub(crate) fn pool_snapshots_csv_header() -> &'static str {
    "id,network,pool_address,protocol_address,protocol_name,token0_address,token0_symbol,token1_address,token1_symbol,reserve0,reserve1,tvl_usd,snapshot_timestamp,block_number,dataset_version_id,created_at\n"
}

pub(crate) fn write_pool_snapshots_csv_rows<W: Write>(
    records: &[PoolSnapshot],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.id,
            csv_escape(&r.network),
            csv_escape(&r.pool_address),
            csv_escape(&r.protocol_address),
            r.protocol_name.as_deref().unwrap_or(""),
            csv_escape(&r.token0_address),
            r.token0_symbol.as_deref().unwrap_or(""),
            csv_escape(&r.token1_address),
            r.token1_symbol.as_deref().unwrap_or(""),
            r.reserve0,
            r.reserve1,
            r.tvl_usd
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.snapshot_timestamp,
            r.block_number.map(|v| v.to_string()).unwrap_or_default(),
            r.dataset_version_id
                .map(|u| u.to_string())
                .unwrap_or_default(),
            r.created_at.to_rfc3339(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// wallet_ledger (tax CSV)
// ---------------------------------------------------------------------------

pub(crate) fn wallet_ledger_tax_csv_header() -> &'static str {
    "Date,Type,Sent_Asset,Sent_Amount,Received_Asset,Received_Amount,Fee_Asset,Fee_Amount,Cost_Basis,Proceeds,Gain_Loss,Tx_Hash,Network\n"
}

pub(crate) fn write_wallet_ledger_tax_csv_rows<W: Write>(
    records: &[WalletLedgerRecord],
    w: &mut W,
) -> std::io::Result<()> {
    for r in records {
        let date = chrono::DateTime::from_timestamp(r.timestamp, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| r.timestamp.to_string());

        let (sent_asset, sent_amount, recv_asset, recv_amount) = if r.amount < BigDecimal::from(0) {
            (
                r.asset_symbol.as_str(),
                r.amount.abs().to_string(),
                "",
                String::new(),
            )
        } else {
            (
                "",
                String::new(),
                r.asset_symbol.as_str(),
                r.amount.to_string(),
            )
        };

        let fee_asset = r.fee_asset.as_deref().unwrap_or("");
        let fee_amount = r
            .fee_amount
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let cost_basis = r
            .cost_basis
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();
        let proceeds = r
            .proceeds
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_default();

        let gain_loss = match (&r.proceeds, &r.cost_basis) {
            (Some(p), Some(c)) => (p - c).to_string(),
            _ => String::new(),
        };

        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            csv_escape(&date),
            csv_escape(&r.entry_type),
            csv_escape(sent_asset),
            sent_amount,
            csv_escape(recv_asset),
            recv_amount,
            csv_escape(fee_asset),
            fee_amount,
            cost_basis,
            proceeds,
            gain_loss,
            csv_escape(&r.tx_hash),
            csv_escape(&r.network),
        )?;
    }
    Ok(())
}
