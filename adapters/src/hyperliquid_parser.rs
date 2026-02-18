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
}
