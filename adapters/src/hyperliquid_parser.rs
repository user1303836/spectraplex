use bigdecimal::BigDecimal;
use spectraplex_core::models::{EntryType, LedgerEntry, Transaction};
use std::str::FromStr;
use tracing::warn;

use crate::deterministic_id;
use crate::hyperliquid::{HlFill, HlFundingEntry, HlLedgerUpdate};

pub fn parse_hyperliquid_transaction(tx: &Transaction) -> anyhow::Result<Vec<LedgerEntry>> {
    let raw = &tx.raw_metadata;
    let entry_type = raw["type"].as_str().unwrap_or("unknown");

    match entry_type {
        "fill" => parse_fill(tx, &raw["data"]),
        "funding" => parse_funding(tx, &raw["data"]),
        "ledger_update" => parse_ledger_update(tx, &raw["data"]),
        other => {
            warn!(tx_hash = %tx.tx_hash, r#type = %other, "Unknown Hyperliquid transaction type");
            Ok(vec![])
        }
    }
}

fn parse_fill(tx: &Transaction, data: &serde_json::Value) -> anyhow::Result<Vec<LedgerEntry>> {
    let fill: HlFill = serde_json::from_value(data.clone())?;
    let mut entries = Vec::new();
    let mut entry_index: u32 = 0;

    let size = BigDecimal::from_str(&fill.sz).unwrap_or_default();
    let price = BigDecimal::from_str(&fill.px).unwrap_or_default();

    // The trade itself: amount is the size, fiat_value is size * price
    let fiat_value = &size * &price;

    // Determine sign based on side: B (buy) = positive, A/S (sell) = negative
    let signed_size = if fill.side == "B" { size } else { -size };

    entries.push(LedgerEntry {
        id: deterministic_id(tx.id, entry_index),
        transaction_id: tx.id,
        user_id: tx.user_id,
        wallet_address: tx.wallet_address.clone(),
        asset_symbol: fill.coin.clone(),
        amount: signed_size,
        entry_type: EntryType::Trade,
        fiat_value: Some(fiat_value),
    });
    entry_index += 1;

    // Fee entry (if present)
    if let Some(ref fee_str) = fill.fee {
        let fee = BigDecimal::from_str(fee_str).unwrap_or_default();
        if fee != BigDecimal::from(0) {
            let fee_token = fill.fee_token.as_deref().unwrap_or("USDC");
            entries.push(LedgerEntry {
                id: deterministic_id(tx.id, entry_index),
                transaction_id: tx.id,
                user_id: tx.user_id,
                wallet_address: tx.wallet_address.clone(),
                asset_symbol: fee_token.to_string(),
                amount: -fee.abs(), // Fees are always outgoing
                entry_type: EntryType::Fee,
                fiat_value: None,
            });
            entry_index += 1;
        }
    }

    // Closed PnL as income (if nonzero)
    if let Some(ref pnl_str) = fill.closed_pnl {
        let pnl = BigDecimal::from_str(pnl_str).unwrap_or_default();
        if pnl != BigDecimal::from(0) {
            entries.push(LedgerEntry {
                id: deterministic_id(tx.id, entry_index),
                transaction_id: tx.id,
                user_id: tx.user_id,
                wallet_address: tx.wallet_address.clone(),
                asset_symbol: "USDC".to_string(),
                amount: pnl,
                entry_type: EntryType::Income,
                fiat_value: None,
            });
            entry_index += 1;
        }
    }

    let _ = entry_index;
    Ok(entries)
}

fn parse_funding(tx: &Transaction, data: &serde_json::Value) -> anyhow::Result<Vec<LedgerEntry>> {
    let funding: HlFundingEntry = serde_json::from_value(data.clone())?;
    let amount = BigDecimal::from_str(&funding.usdc).unwrap_or_default();

    // Funding payments: positive = received, negative = paid
    Ok(vec![LedgerEntry {
        id: deterministic_id(tx.id, 0),
        transaction_id: tx.id,
        user_id: tx.user_id,
        wallet_address: tx.wallet_address.clone(),
        asset_symbol: "USDC".to_string(),
        amount,
        entry_type: EntryType::Fee,
        fiat_value: None,
    }])
}

fn parse_ledger_update(
    tx: &Transaction,
    data: &serde_json::Value,
) -> anyhow::Result<Vec<LedgerEntry>> {
    let update: HlLedgerUpdate = serde_json::from_value(data.clone())?;
    let delta = &update.delta;

    let delta_type = delta["type"].as_str().unwrap_or("unknown");

    match delta_type {
        "deposit" => {
            let usdc = delta["usdc"].as_str().unwrap_or("0");
            let amount = BigDecimal::from_str(usdc).unwrap_or_default();
            Ok(vec![LedgerEntry {
                id: deterministic_id(tx.id, 0),
                transaction_id: tx.id,
                user_id: tx.user_id,
                wallet_address: tx.wallet_address.clone(),
                asset_symbol: "USDC".to_string(),
                amount,
                entry_type: EntryType::Transfer,
                fiat_value: None,
            }])
        }
        "withdraw" => {
            let usdc = delta["usdc"].as_str().unwrap_or("0");
            let amount = BigDecimal::from_str(usdc).unwrap_or_default();
            Ok(vec![LedgerEntry {
                id: deterministic_id(tx.id, 0),
                transaction_id: tx.id,
                user_id: tx.user_id,
                wallet_address: tx.wallet_address.clone(),
                asset_symbol: "USDC".to_string(),
                amount: -amount.abs(),
                entry_type: EntryType::Transfer,
                fiat_value: None,
            }])
        }
        "liquidation" => {
            // Liquidation can have complex delta structures; store a generic entry
            let usdc = delta["usdc"]
                .as_str()
                .or_else(|| delta["accountValue"].as_str())
                .unwrap_or("0");
            let amount = BigDecimal::from_str(usdc).unwrap_or_default();
            Ok(vec![LedgerEntry {
                id: deterministic_id(tx.id, 0),
                transaction_id: tx.id,
                user_id: tx.user_id,
                wallet_address: tx.wallet_address.clone(),
                asset_symbol: "USDC".to_string(),
                amount,
                entry_type: EntryType::Trade,
                fiat_value: None,
            }])
        }
        _ => {
            warn!(tx_hash = %tx.tx_hash, delta_type = %delta_type, "Unknown ledger update type");
            Ok(vec![])
        }
    }
}

// ---------------------------------------------------------------------------
// Token Transfer extraction (P3-W2)
// ---------------------------------------------------------------------------

use chrono::Utc;
use spectraplex_core::materializer::{
    DatasetDescriptor, DatasetName, HlFillRecord, HlFundingPayment, HlPositionChange, Materializer,
    NativeBalanceDelta, TokenTransfer,
};
use spectraplex_core::v2::ChainFamily;
use uuid::Uuid;

/// Extract Hyperliquid token transfers from a raw transaction.
///
/// Deposits and withdrawals are modeled as USDC transfers between an external
/// source/destination and the user's Hyperliquid account.
pub fn extract_hyperliquid_token_transfers(
    raw_tx_id: Option<Uuid>,
    network: &str,
    wallet_address: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<TokenTransfer> {
    let entry_type = raw_metadata["type"].as_str().unwrap_or("unknown");

    match entry_type {
        "ledger_update" => {
            let data = &raw_metadata["data"];
            let delta = &data["delta"];
            let delta_type = delta["type"].as_str().unwrap_or("unknown");

            match delta_type {
                "deposit" => {
                    let usdc = delta["usdc"].as_str().unwrap_or("0");
                    let amount = BigDecimal::from_str(usdc).unwrap_or_default();
                    if amount == BigDecimal::from(0) {
                        return vec![];
                    }
                    vec![TokenTransfer {
                        id: Uuid::new_v4(),
                        raw_transaction_id: raw_tx_id,
                        network: network.to_string(),
                        token_address: "USDC".to_string(),
                        token_symbol: Some("USDC".to_string()),
                        from_address: "external".to_string(),
                        to_address: wallet_address.to_string(),
                        amount,
                        decimals: 6,
                        dataset_version_id: None,
                        created_at: Utc::now(),
                    }]
                }
                "withdraw" => {
                    let usdc = delta["usdc"].as_str().unwrap_or("0");
                    let amount = BigDecimal::from_str(usdc).unwrap_or_default().abs();
                    if amount == BigDecimal::from(0) {
                        return vec![];
                    }
                    vec![TokenTransfer {
                        id: Uuid::new_v4(),
                        raw_transaction_id: raw_tx_id,
                        network: network.to_string(),
                        token_address: "USDC".to_string(),
                        token_symbol: Some("USDC".to_string()),
                        from_address: wallet_address.to_string(),
                        to_address: "external".to_string(),
                        amount,
                        decimals: 6,
                        dataset_version_id: None,
                        created_at: Utc::now(),
                    }]
                }
                _ => vec![],
            }
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Native Balance Delta extraction (P3-W2)
// ---------------------------------------------------------------------------

/// Extract Hyperliquid native balance deltas from a raw transaction.
///
/// On Hyperliquid, the "native" token is USDC (the settlement currency).
/// Balance deltas are extracted from fill and funding events as changes to the
/// account's USDC balance.
pub fn extract_hyperliquid_native_balance_deltas(
    raw_tx_id: Option<Uuid>,
    network: &str,
    wallet_address: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<NativeBalanceDelta> {
    let entry_type = raw_metadata["type"].as_str().unwrap_or("unknown");

    match entry_type {
        "fill" => {
            let data = &raw_metadata["data"];
            let fill: HlFill = match serde_json::from_value(data.clone()) {
                Ok(f) => f,
                Err(_) => return vec![],
            };

            // Fee and closed PnL represent USDC balance changes
            let fee = fill
                .fee
                .as_deref()
                .and_then(|f| BigDecimal::from_str(f).ok())
                .unwrap_or_default();
            let pnl = fill
                .closed_pnl
                .as_deref()
                .and_then(|p| BigDecimal::from_str(p).ok())
                .unwrap_or_default();

            // Total USDC delta from this fill = -fee + closed_pnl
            let delta = &pnl - &fee.abs();

            if delta == BigDecimal::from(0) {
                return vec![];
            }

            vec![NativeBalanceDelta {
                id: Uuid::new_v4(),
                raw_transaction_id: raw_tx_id,
                network: network.to_string(),
                account_address: wallet_address.to_string(),
                native_token: "USDC".to_string(),
                pre_balance: BigDecimal::from(0), // Not available from fill data
                post_balance: BigDecimal::from(0), // Not available from fill data
                delta,
                is_fee_payer: true, // The user always pays fees on HL
                dataset_version_id: None,
                created_at: Utc::now(),
            }]
        }
        "funding" => {
            let data = &raw_metadata["data"];
            let funding: HlFundingEntry = match serde_json::from_value(data.clone()) {
                Ok(f) => f,
                Err(_) => return vec![],
            };

            let delta = BigDecimal::from_str(&funding.usdc).unwrap_or_default();
            if delta == BigDecimal::from(0) {
                return vec![];
            }

            vec![NativeBalanceDelta {
                id: Uuid::new_v4(),
                raw_transaction_id: raw_tx_id,
                network: network.to_string(),
                account_address: wallet_address.to_string(),
                native_token: "USDC".to_string(),
                pre_balance: BigDecimal::from(0),
                post_balance: BigDecimal::from(0),
                delta,
                is_fee_payer: false,
                dataset_version_id: None,
                created_at: Utc::now(),
            }]
        }
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// Materializer implementations
// ---------------------------------------------------------------------------

/// Materializer wrapper for the Hyperliquid ledger parser.
pub struct HyperliquidLedgerMaterializer;

impl Materializer for HyperliquidLedgerMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::LedgerEntries
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:hl_ledger_v1_c5a2e8b3"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Hyperliquid
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Hyperliquid ledger entries: fills, funding payments, deposits, and withdrawals"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// Materializer for Hyperliquid token transfers (deposits/withdrawals).
pub struct HyperliquidTokenTransferMaterializer;

impl Materializer for HyperliquidTokenTransferMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::TokenTransfers
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:hl_token_transfers_v1_e7f2a4d8"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Hyperliquid
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description: "Hyperliquid token transfers: USDC deposits and withdrawals".to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// Materializer for Hyperliquid native balance deltas.
pub struct HyperliquidNativeBalanceDeltaMaterializer;

impl Materializer for HyperliquidNativeBalanceDeltaMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::NativeBalanceDeltas
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:hl_native_deltas_v1_f3d6c9a2"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Hyperliquid
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Hyperliquid native balance deltas: USDC balance changes from fills and funding"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

// ---------------------------------------------------------------------------
// HlFillRecord extraction (P3-W4)
// ---------------------------------------------------------------------------

/// Extract a normalized Hyperliquid fill record from raw transaction metadata.
///
/// Produces a fill record from "fill"-type Bronze raw_transactions.
pub fn extract_hl_fill_records(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<HlFillRecord> {
    let entry_type = raw_metadata["type"].as_str().unwrap_or("unknown");

    if entry_type != "fill" {
        return vec![];
    }

    let data = &raw_metadata["data"];
    let fill: HlFill = match serde_json::from_value(data.clone()) {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let price = BigDecimal::from_str(&fill.px).unwrap_or_default();
    let size = BigDecimal::from_str(&fill.sz).unwrap_or_default();

    let closed_pnl = fill
        .closed_pnl
        .as_deref()
        .and_then(|s| BigDecimal::from_str(s).ok());
    let fee = fill
        .fee
        .as_deref()
        .and_then(|s| BigDecimal::from_str(s).ok());

    vec![HlFillRecord {
        id: Uuid::new_v4(),
        raw_transaction_id: raw_tx_id,
        network: network.to_string(),
        coin: fill.coin,
        side: fill.side,
        price,
        size,
        direction: fill.dir,
        closed_pnl,
        fee,
        fee_token: fill.fee_token,
        fill_time: fill.time as i64,
        order_id: fill.oid.map(|v| v as i64),
        trade_id: fill.tid.map(|v| v as i64),
        dataset_version_id: None,
        created_at: Utc::now(),
    }]
}

// ---------------------------------------------------------------------------
// HlFundingPayment extraction (P3-W4)
// ---------------------------------------------------------------------------

/// Extract a normalized Hyperliquid funding payment from raw transaction metadata.
///
/// Produces a funding payment record from "funding"-type Bronze raw_transactions.
pub fn extract_hl_funding_payments(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<HlFundingPayment> {
    let entry_type = raw_metadata["type"].as_str().unwrap_or("unknown");

    if entry_type != "funding" {
        return vec![];
    }

    let data = &raw_metadata["data"];
    let funding: HlFundingEntry = match serde_json::from_value(data.clone()) {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let amount = BigDecimal::from_str(&funding.usdc).unwrap_or_default();
    let funding_rate = funding
        .funding_rate
        .as_deref()
        .and_then(|s| BigDecimal::from_str(s).ok());

    vec![HlFundingPayment {
        id: Uuid::new_v4(),
        raw_transaction_id: raw_tx_id,
        network: network.to_string(),
        coin: funding.coin,
        amount,
        funding_rate,
        payment_time: funding.time as i64,
        dataset_version_id: None,
        created_at: Utc::now(),
    }]
}

// ---------------------------------------------------------------------------
// HlPositionChange extraction (P3-W4)
// ---------------------------------------------------------------------------

/// Extract a normalized Hyperliquid position change from raw transaction metadata.
///
/// Position changes are derived from fills (and liquidations). Each fill that
/// opens, increases, or closes a position produces a position change record.
pub fn extract_hl_position_changes(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<HlPositionChange> {
    let entry_type = raw_metadata["type"].as_str().unwrap_or("unknown");

    match entry_type {
        "fill" => extract_position_change_from_fill(raw_tx_id, network, raw_metadata),
        "ledger_update" => {
            extract_position_change_from_liquidation(raw_tx_id, network, raw_metadata)
        }
        _ => vec![],
    }
}

fn extract_position_change_from_fill(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<HlPositionChange> {
    let data = &raw_metadata["data"];
    let fill: HlFill = match serde_json::from_value(data.clone()) {
        Ok(f) => f,
        Err(_) => return vec![],
    };

    let size = BigDecimal::from_str(&fill.sz).unwrap_or_default();
    let price = BigDecimal::from_str(&fill.px).unwrap_or_default();

    // Determine signed size_delta: buy = positive, sell = negative
    let size_delta = if fill.side == "B" { size } else { -size };

    vec![HlPositionChange {
        id: Uuid::new_v4(),
        raw_transaction_id: raw_tx_id,
        network: network.to_string(),
        coin: fill.coin,
        side: fill.side,
        size_delta,
        price,
        direction: fill.dir,
        source_event: "fill".to_string(),
        dataset_version_id: None,
        created_at: Utc::now(),
    }]
}

fn extract_position_change_from_liquidation(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<HlPositionChange> {
    let data = &raw_metadata["data"];
    let delta = &data["delta"];
    let delta_type = delta["type"].as_str().unwrap_or("unknown");

    if delta_type != "liquidation" {
        return vec![];
    }

    // Liquidation events may contain leveraged position info
    let coin = delta["coin"].as_str().unwrap_or("UNKNOWN");
    let size_str = delta["sz"].as_str().unwrap_or("0");
    let price_str = delta["px"].as_str().unwrap_or("0");
    let side = delta["side"].as_str().unwrap_or("A");

    let size = BigDecimal::from_str(size_str).unwrap_or_default();
    let price = BigDecimal::from_str(price_str).unwrap_or_default();

    if size == BigDecimal::from(0) {
        return vec![];
    }

    // Liquidation closes a position, so size_delta is opposite to position side
    let size_delta = if side == "B" { size } else { -size };

    vec![HlPositionChange {
        id: Uuid::new_v4(),
        raw_transaction_id: raw_tx_id,
        network: network.to_string(),
        coin: coin.to_string(),
        side: side.to_string(),
        size_delta,
        price,
        direction: Some("Liquidation".to_string()),
        source_event: "liquidation".to_string(),
        dataset_version_id: None,
        created_at: Utc::now(),
    }]
}

// ---------------------------------------------------------------------------
// Materializer implementations (P3-W4)
// ---------------------------------------------------------------------------

/// Materializer for Hyperliquid fill records.
pub struct HlFillMaterializer;

impl Materializer for HlFillMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::HlFills
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:hl_fill_records_v1_a1b2c3d4"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Hyperliquid
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Hyperliquid fill records: normalized trade executions with price, size, fees, and PnL"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// Materializer for Hyperliquid funding payments.
pub struct HlFundingPaymentMaterializer;

impl Materializer for HlFundingPaymentMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::HlFunding
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:hl_funding_payments_v1_e5f6g7h8"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Hyperliquid
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Hyperliquid funding payments: periodic funding rate payments received or paid"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// Materializer for Hyperliquid position changes.
pub struct HlPositionChangeMaterializer;

impl Materializer for HlPositionChangeMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::Positions
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:hl_position_changes_v1_i9j0k1l2"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Hyperliquid
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Hyperliquid position changes: state changes derived from fills and liquidations"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spectraplex_core::models::Chain;
    use uuid::Uuid;

    fn make_tx(raw_type: &str, data: serde_json::Value) -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::nil(),
            wallet_address: "0xtest".to_string(),
            timestamp: 1700000000,
            tx_hash: "0xhash".to_string(),
            chain: Chain::Hyperliquid,
            raw_metadata: serde_json::json!({ "type": raw_type, "data": data }),
        }
    }

    #[test]
    fn test_parse_fill_buy() {
        let tx = make_tx(
            "fill",
            serde_json::json!({
                "coin": "ETH",
                "px": "3500.0",
                "sz": "2.0",
                "side": "B",
                "time": 1700000000000u64,
                "hash": "0xfill1",
                "fee": "3.50",
                "feeToken": "USDC",
                "closedPnl": "0.0"
            }),
        );

        let entries = parse_hyperliquid_transaction(&tx).unwrap();
        // Trade + Fee (closedPnl=0 so no income entry)
        assert_eq!(entries.len(), 2);

        let trade = &entries[0];
        assert_eq!(trade.asset_symbol, "ETH");
        assert_eq!(trade.amount, BigDecimal::from_str("2.0").unwrap());
        assert!(matches!(trade.entry_type, EntryType::Trade));
        assert_eq!(
            trade.fiat_value.as_ref().unwrap(),
            &BigDecimal::from_str("7000.0").unwrap()
        );

        let fee = &entries[1];
        assert_eq!(fee.asset_symbol, "USDC");
        assert_eq!(fee.amount, BigDecimal::from_str("-3.50").unwrap());
        assert!(matches!(fee.entry_type, EntryType::Fee));
    }

    #[test]
    fn test_parse_fill_sell_with_pnl() {
        let tx = make_tx(
            "fill",
            serde_json::json!({
                "coin": "BTC",
                "px": "42000.0",
                "sz": "0.5",
                "side": "A",
                "time": 1700000000000u64,
                "hash": "0xfill2",
                "fee": "10.50",
                "feeToken": "USDC",
                "closedPnl": "500.0",
                "dir": "Close Long"
            }),
        );

        let entries = parse_hyperliquid_transaction(&tx).unwrap();
        // Trade + Fee + Income (PnL)
        assert_eq!(entries.len(), 3);

        let trade = &entries[0];
        assert_eq!(trade.amount, BigDecimal::from_str("-0.5").unwrap());

        let income = &entries[2];
        assert_eq!(income.amount, BigDecimal::from_str("500.0").unwrap());
        assert!(matches!(income.entry_type, EntryType::Income));
    }

    #[test]
    fn test_parse_funding() {
        let tx = make_tx(
            "funding",
            serde_json::json!({
                "time": 1700000000000u64,
                "coin": "ETH",
                "usdc": "-2.50",
                "fundingRate": "0.0001"
            }),
        );

        let entries = parse_hyperliquid_transaction(&tx).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "USDC");
        assert_eq!(entries[0].amount, BigDecimal::from_str("-2.50").unwrap());
        assert!(matches!(entries[0].entry_type, EntryType::Fee));
    }

    #[test]
    fn test_parse_deposit() {
        let tx = make_tx(
            "ledger_update",
            serde_json::json!({
                "time": 1700000000000u64,
                "hash": "0xdep1",
                "delta": { "type": "deposit", "usdc": "10000.0" }
            }),
        );

        let entries = parse_hyperliquid_transaction(&tx).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].amount, BigDecimal::from_str("10000.0").unwrap());
        assert!(matches!(entries[0].entry_type, EntryType::Transfer));
    }

    #[test]
    fn test_parse_withdrawal() {
        let tx = make_tx(
            "ledger_update",
            serde_json::json!({
                "time": 1700000000000u64,
                "hash": "0xwith1",
                "delta": { "type": "withdraw", "usdc": "5000.0" }
            }),
        );

        let entries = parse_hyperliquid_transaction(&tx).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].amount, BigDecimal::from_str("-5000.0").unwrap());
        assert!(matches!(entries[0].entry_type, EntryType::Transfer));
    }

    #[test]
    fn test_parse_unknown_type() {
        let tx = make_tx("unknown_type", serde_json::json!({}));
        let entries = parse_hyperliquid_transaction(&tx).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn hyperliquid_materializer_contract() {
        let m = HyperliquidLedgerMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::LedgerEntries);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Hyperliquid);
        assert!(!m.parser_hash().is_empty());
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::LedgerEntries);
        assert_eq!(desc.chain_families, vec![ChainFamily::Hyperliquid]);
    }

    #[test]
    fn hyperliquid_token_transfer_materializer_contract() {
        let m = HyperliquidTokenTransferMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::TokenTransfers);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Hyperliquid);
        assert!(!m.parser_hash().is_empty());
        assert_ne!(m.parser_hash(), HyperliquidLedgerMaterializer.parser_hash());
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::TokenTransfers);
    }

    #[test]
    fn hyperliquid_native_balance_delta_materializer_contract() {
        let m = HyperliquidNativeBalanceDeltaMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::NativeBalanceDeltas);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Hyperliquid);
        assert!(!m.parser_hash().is_empty());
        assert_ne!(m.parser_hash(), HyperliquidLedgerMaterializer.parser_hash());
        assert_ne!(
            m.parser_hash(),
            HyperliquidTokenTransferMaterializer.parser_hash()
        );
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::NativeBalanceDeltas);
    }

    #[test]
    fn hyperliquid_all_materializers_have_distinct_hashes() {
        use std::collections::HashSet;
        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(HyperliquidLedgerMaterializer),
            Box::new(HyperliquidTokenTransferMaterializer),
            Box::new(HyperliquidNativeBalanceDeltaMaterializer),
        ];
        let hashes: HashSet<&str> = materializers.iter().map(|m| m.parser_hash()).collect();
        assert_eq!(
            hashes.len(),
            3,
            "all 3 HL materializers must have distinct hashes"
        );
    }

    #[test]
    fn test_extract_hl_token_transfer_deposit() {
        let metadata = serde_json::json!({
            "type": "ledger_update",
            "data": {
                "time": 1700000000000u64,
                "hash": "0xdep1",
                "delta": { "type": "deposit", "usdc": "10000.0" }
            }
        });

        let transfers = extract_hyperliquid_token_transfers(
            Some(Uuid::new_v4()),
            "hypercore-mainnet",
            "0xtest",
            &metadata,
        );

        assert_eq!(transfers.len(), 1);
        let t = &transfers[0];
        assert_eq!(t.from_address, "external");
        assert_eq!(t.to_address, "0xtest");
        assert_eq!(t.token_symbol, Some("USDC".to_string()));
        assert_eq!(t.amount, BigDecimal::from_str("10000.0").unwrap());
    }

    #[test]
    fn test_extract_hl_token_transfer_withdrawal() {
        let metadata = serde_json::json!({
            "type": "ledger_update",
            "data": {
                "time": 1700000000000u64,
                "hash": "0xwith1",
                "delta": { "type": "withdraw", "usdc": "5000.0" }
            }
        });

        let transfers =
            extract_hyperliquid_token_transfers(None, "hypercore-mainnet", "0xuser", &metadata);

        assert_eq!(transfers.len(), 1);
        let t = &transfers[0];
        assert_eq!(t.from_address, "0xuser");
        assert_eq!(t.to_address, "external");
        assert_eq!(t.amount, BigDecimal::from_str("5000.0").unwrap());
    }

    #[test]
    fn test_extract_hl_token_transfer_fill_returns_none() {
        let metadata = serde_json::json!({
            "type": "fill",
            "data": {
                "coin": "ETH",
                "px": "3500.0",
                "sz": "2.0",
                "side": "B",
                "time": 1700000000000u64,
                "hash": "0xfill1",
            }
        });

        let transfers =
            extract_hyperliquid_token_transfers(None, "hypercore-mainnet", "0xtest", &metadata);
        assert!(transfers.is_empty(), "fills are not token transfers");
    }

    #[test]
    fn test_extract_hl_native_delta_fill() {
        let metadata = serde_json::json!({
            "type": "fill",
            "data": {
                "coin": "ETH",
                "px": "3500.0",
                "sz": "2.0",
                "side": "B",
                "time": 1700000000000u64,
                "hash": "0xfill1",
                "fee": "3.50",
                "feeToken": "USDC",
                "closedPnl": "100.0"
            }
        });

        let deltas = extract_hyperliquid_native_balance_deltas(
            Some(Uuid::new_v4()),
            "hypercore-mainnet",
            "0xuser",
            &metadata,
        );

        assert_eq!(deltas.len(), 1);
        let d = &deltas[0];
        assert_eq!(d.account_address, "0xuser");
        assert_eq!(d.native_token, "USDC");
        // delta = pnl - |fee| = 100.0 - 3.50 = 96.50
        assert_eq!(d.delta, BigDecimal::from_str("96.50").unwrap());
        assert!(d.is_fee_payer);
    }

    #[test]
    fn test_extract_hl_native_delta_funding() {
        let metadata = serde_json::json!({
            "type": "funding",
            "data": {
                "time": 1700000000000u64,
                "coin": "ETH",
                "usdc": "-2.50",
                "fundingRate": "0.0001"
            }
        });

        let deltas = extract_hyperliquid_native_balance_deltas(
            None,
            "hypercore-mainnet",
            "0xuser",
            &metadata,
        );

        assert_eq!(deltas.len(), 1);
        let d = &deltas[0];
        assert_eq!(d.delta, BigDecimal::from_str("-2.50").unwrap());
        assert!(!d.is_fee_payer);
    }

    #[test]
    fn test_extract_hl_native_delta_unknown_type() {
        let metadata = serde_json::json!({
            "type": "ledger_update",
            "data": {
                "time": 1700000000000u64,
                "delta": { "type": "deposit", "usdc": "100.0" }
            }
        });

        let deltas = extract_hyperliquid_native_balance_deltas(
            None,
            "hypercore-mainnet",
            "0xuser",
            &metadata,
        );
        assert!(
            deltas.is_empty(),
            "ledger_update does not produce native balance deltas"
        );
    }

    // -- HlFillRecord extraction tests (P3-W4) --

    #[test]
    fn test_extract_hl_fill_record_buy() {
        let metadata = serde_json::json!({
            "type": "fill",
            "data": {
                "coin": "ETH",
                "px": "3500.0",
                "sz": "2.0",
                "side": "B",
                "time": 1700000000000u64,
                "hash": "0xfill1",
                "fee": "3.50",
                "feeToken": "USDC",
                "closedPnl": "0.0",
                "dir": "Open Long",
                "oid": 12345,
                "tid": 67890
            }
        });

        let fills = extract_hl_fill_records(Some(Uuid::new_v4()), "hypercore-mainnet", &metadata);

        assert_eq!(fills.len(), 1);
        let f = &fills[0];
        assert_eq!(f.coin, "ETH");
        assert_eq!(f.side, "B");
        assert_eq!(f.price, BigDecimal::from_str("3500.0").unwrap());
        assert_eq!(f.size, BigDecimal::from_str("2.0").unwrap());
        assert_eq!(f.direction, Some("Open Long".to_string()));
        assert_eq!(f.fee, Some(BigDecimal::from_str("3.50").unwrap()));
        assert_eq!(f.fee_token, Some("USDC".to_string()));
        assert_eq!(f.fill_time, 1700000000000);
        assert_eq!(f.order_id, Some(12345));
        assert_eq!(f.trade_id, Some(67890));
        assert_eq!(f.network, "hypercore-mainnet");
    }

    #[test]
    fn test_extract_hl_fill_record_sell_with_pnl() {
        let metadata = serde_json::json!({
            "type": "fill",
            "data": {
                "coin": "BTC",
                "px": "42000.0",
                "sz": "0.5",
                "side": "A",
                "time": 1700000000000u64,
                "hash": "0xfill2",
                "fee": "10.50",
                "feeToken": "USDC",
                "closedPnl": "500.0",
                "dir": "Close Long"
            }
        });

        let fills = extract_hl_fill_records(None, "hypercore-mainnet", &metadata);

        assert_eq!(fills.len(), 1);
        let f = &fills[0];
        assert_eq!(f.coin, "BTC");
        assert_eq!(f.side, "A");
        assert_eq!(f.closed_pnl, Some(BigDecimal::from_str("500.0").unwrap()));
        assert_eq!(f.direction, Some("Close Long".to_string()));
    }

    #[test]
    fn test_extract_hl_fill_record_funding_returns_none() {
        let metadata = serde_json::json!({
            "type": "funding",
            "data": {
                "time": 1700000000000u64,
                "coin": "ETH",
                "usdc": "-2.50",
                "fundingRate": "0.0001"
            }
        });

        let fills = extract_hl_fill_records(None, "hypercore-mainnet", &metadata);
        assert!(
            fills.is_empty(),
            "funding events should not produce fill records"
        );
    }

    // -- HlFundingPayment extraction tests (P3-W4) --

    #[test]
    fn test_extract_hl_funding_payment() {
        let metadata = serde_json::json!({
            "type": "funding",
            "data": {
                "time": 1700000000000u64,
                "coin": "ETH",
                "usdc": "-2.50",
                "fundingRate": "0.0001"
            }
        });

        let payments =
            extract_hl_funding_payments(Some(Uuid::new_v4()), "hypercore-mainnet", &metadata);

        assert_eq!(payments.len(), 1);
        let p = &payments[0];
        assert_eq!(p.coin, "ETH");
        assert_eq!(p.amount, BigDecimal::from_str("-2.50").unwrap());
        assert_eq!(
            p.funding_rate,
            Some(BigDecimal::from_str("0.0001").unwrap())
        );
        assert_eq!(p.payment_time, 1700000000000);
        assert_eq!(p.network, "hypercore-mainnet");
    }

    #[test]
    fn test_extract_hl_funding_payment_positive() {
        let metadata = serde_json::json!({
            "type": "funding",
            "data": {
                "time": 1700000001000u64,
                "coin": "BTC",
                "usdc": "5.00",
                "fundingRate": "-0.0002"
            }
        });

        let payments = extract_hl_funding_payments(None, "hypercore-mainnet", &metadata);

        assert_eq!(payments.len(), 1);
        let p = &payments[0];
        assert_eq!(p.coin, "BTC");
        assert_eq!(p.amount, BigDecimal::from_str("5.00").unwrap());
    }

    #[test]
    fn test_extract_hl_funding_payment_fill_returns_none() {
        let metadata = serde_json::json!({
            "type": "fill",
            "data": {
                "coin": "ETH",
                "px": "3500.0",
                "sz": "2.0",
                "side": "B",
                "time": 1700000000000u64,
                "hash": "0xfill1"
            }
        });

        let payments = extract_hl_funding_payments(None, "hypercore-mainnet", &metadata);
        assert!(
            payments.is_empty(),
            "fills should not produce funding payments"
        );
    }

    // -- HlPositionChange extraction tests (P3-W4) --

    #[test]
    fn test_extract_hl_position_change_from_fill_buy() {
        let metadata = serde_json::json!({
            "type": "fill",
            "data": {
                "coin": "ETH",
                "px": "3500.0",
                "sz": "2.0",
                "side": "B",
                "time": 1700000000000u64,
                "hash": "0xfill1",
                "dir": "Open Long"
            }
        });

        let changes =
            extract_hl_position_changes(Some(Uuid::new_v4()), "hypercore-mainnet", &metadata);

        assert_eq!(changes.len(), 1);
        let c = &changes[0];
        assert_eq!(c.coin, "ETH");
        assert_eq!(c.side, "B");
        assert_eq!(c.size_delta, BigDecimal::from_str("2.0").unwrap());
        assert_eq!(c.price, BigDecimal::from_str("3500.0").unwrap());
        assert_eq!(c.direction, Some("Open Long".to_string()));
        assert_eq!(c.source_event, "fill");
    }

    #[test]
    fn test_extract_hl_position_change_from_fill_sell() {
        let metadata = serde_json::json!({
            "type": "fill",
            "data": {
                "coin": "BTC",
                "px": "42000.0",
                "sz": "0.5",
                "side": "A",
                "time": 1700000000000u64,
                "hash": "0xfill2",
                "dir": "Close Long"
            }
        });

        let changes = extract_hl_position_changes(None, "hypercore-mainnet", &metadata);

        assert_eq!(changes.len(), 1);
        let c = &changes[0];
        assert_eq!(c.coin, "BTC");
        assert_eq!(c.side, "A");
        assert_eq!(c.size_delta, BigDecimal::from_str("-0.5").unwrap());
        assert_eq!(c.direction, Some("Close Long".to_string()));
        assert_eq!(c.source_event, "fill");
    }

    #[test]
    fn test_extract_hl_position_change_from_liquidation() {
        let metadata = serde_json::json!({
            "type": "ledger_update",
            "data": {
                "time": 1700000000000u64,
                "hash": "0xliq1",
                "delta": {
                    "type": "liquidation",
                    "coin": "ETH",
                    "sz": "5.0",
                    "px": "3000.0",
                    "side": "A"
                }
            }
        });

        let changes =
            extract_hl_position_changes(Some(Uuid::new_v4()), "hypercore-mainnet", &metadata);

        assert_eq!(changes.len(), 1);
        let c = &changes[0];
        assert_eq!(c.coin, "ETH");
        assert_eq!(c.side, "A");
        assert_eq!(c.size_delta, BigDecimal::from_str("-5.0").unwrap());
        assert_eq!(c.direction, Some("Liquidation".to_string()));
        assert_eq!(c.source_event, "liquidation");
    }

    #[test]
    fn test_extract_hl_position_change_from_deposit_returns_none() {
        let metadata = serde_json::json!({
            "type": "ledger_update",
            "data": {
                "time": 1700000000000u64,
                "hash": "0xdep1",
                "delta": { "type": "deposit", "usdc": "10000.0" }
            }
        });

        let changes = extract_hl_position_changes(None, "hypercore-mainnet", &metadata);
        assert!(
            changes.is_empty(),
            "deposits should not produce position changes"
        );
    }

    #[test]
    fn test_extract_hl_position_change_from_funding_returns_none() {
        let metadata = serde_json::json!({
            "type": "funding",
            "data": {
                "time": 1700000000000u64,
                "coin": "ETH",
                "usdc": "-2.50",
                "fundingRate": "0.0001"
            }
        });

        let changes = extract_hl_position_changes(None, "hypercore-mainnet", &metadata);
        assert!(
            changes.is_empty(),
            "funding events should not produce position changes"
        );
    }

    // -- P3-W4 Materializer contract tests --

    #[test]
    fn hl_fill_materializer_contract() {
        let m = HlFillMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::HlFills);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Hyperliquid);
        assert!(!m.parser_hash().is_empty());
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::HlFills);
        assert_eq!(desc.chain_families, vec![ChainFamily::Hyperliquid]);
    }

    #[test]
    fn hl_funding_payment_materializer_contract() {
        let m = HlFundingPaymentMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::HlFunding);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Hyperliquid);
        assert!(!m.parser_hash().is_empty());
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::HlFunding);
    }

    #[test]
    fn hl_position_change_materializer_contract() {
        let m = HlPositionChangeMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::Positions);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Hyperliquid);
        assert!(!m.parser_hash().is_empty());
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::Positions);
    }

    #[test]
    fn hl_all_p3w4_materializers_have_distinct_hashes() {
        use std::collections::HashSet;
        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(HlFillMaterializer),
            Box::new(HlFundingPaymentMaterializer),
            Box::new(HlPositionChangeMaterializer),
        ];
        let hashes: HashSet<&str> = materializers.iter().map(|m| m.parser_hash()).collect();
        assert_eq!(
            hashes.len(),
            3,
            "all 3 P3-W4 materializers must have distinct hashes"
        );
    }

    #[test]
    fn hl_all_materializers_have_distinct_hashes_across_all_hl() {
        use std::collections::HashSet;
        let materializers: Vec<Box<dyn Materializer>> = vec![
            // P3-W1 / existing
            Box::new(HyperliquidLedgerMaterializer),
            // P3-W2
            Box::new(HyperliquidTokenTransferMaterializer),
            Box::new(HyperliquidNativeBalanceDeltaMaterializer),
            // P3-W4
            Box::new(HlFillMaterializer),
            Box::new(HlFundingPaymentMaterializer),
            Box::new(HlPositionChangeMaterializer),
        ];
        let hashes: HashSet<&str> = materializers.iter().map(|m| m.parser_hash()).collect();
        assert_eq!(
            hashes.len(),
            6,
            "all 6 HL materializers must have distinct hashes"
        );
    }
}
