//! Silver→LedgerEntry derivation functions.
//!
//! Produces wallet-scoped `LedgerEntry` records from reusable Silver datasets,
//! proving that ledger generation can be decoupled from direct Bronze parsing
//! while keeping current API output stable.

use bigdecimal::BigDecimal;
use spectraplex_core::materializer::{
    DatasetDescriptor, DatasetName, HlFillRecord, HlFundingPayment, Materializer,
    NativeBalanceDelta, TokenTransfer,
};
use spectraplex_core::models::{Chain, EntryType, LedgerEntry};
use spectraplex_core::v2::ChainFamily;
use uuid::Uuid;

use crate::deterministic_id;

// ---------------------------------------------------------------------------
// Silver record container
// ---------------------------------------------------------------------------

/// Groups Silver dataset records for a single transaction.
///
/// Used by `derive_all_ledger_entries` to dispatch to the appropriate
/// chain-specific derivation functions.
#[derive(Default)]
pub struct SilverRecords {
    pub token_transfers: Vec<TokenTransfer>,
    pub native_balance_deltas: Vec<NativeBalanceDelta>,
    pub hl_fills: Vec<HlFillRecord>,
    pub hl_funding: Vec<HlFundingPayment>,
}

// ---------------------------------------------------------------------------
// Derivation functions
// ---------------------------------------------------------------------------

/// Derive ledger entries from Silver `TokenTransfer` records.
///
/// Filters transfers where `from_address` or `to_address` matches the given
/// wallet, producing `Transfer` entries with signed amounts:
/// - Negative for outgoing (wallet is sender)
/// - Positive for incoming (wallet is receiver)
pub fn derive_ledger_from_token_transfers(
    wallet: &str,
    user_id: Uuid,
    tx_id: Uuid,
    transfers: &[TokenTransfer],
    entry_offset: &mut u32,
) -> Vec<LedgerEntry> {
    let wallet_lower = wallet.to_lowercase();
    let mut entries = Vec::new();

    for transfer in transfers {
        let from_match = transfer.from_address.to_lowercase() == wallet_lower;
        let to_match = transfer.to_address.to_lowercase() == wallet_lower;

        if !from_match && !to_match {
            continue;
        }

        let symbol = transfer
            .token_symbol
            .clone()
            .unwrap_or_else(|| transfer.token_address.clone());

        if from_match {
            entries.push(LedgerEntry {
                id: deterministic_id(tx_id, *entry_offset),
                transaction_id: tx_id,
                user_id,
                wallet_address: wallet.to_string(),
                asset_symbol: symbol.clone(),
                amount: -transfer.amount.clone(),
                entry_type: EntryType::Transfer,
                fiat_value: None,
            });
            *entry_offset += 1;
        }

        if to_match {
            entries.push(LedgerEntry {
                id: deterministic_id(tx_id, *entry_offset),
                transaction_id: tx_id,
                user_id,
                wallet_address: wallet.to_string(),
                asset_symbol: symbol,
                amount: transfer.amount.clone(),
                entry_type: EntryType::Transfer,
                fiat_value: None,
            });
            *entry_offset += 1;
        }
    }

    entries
}

/// Derive ledger entries from Silver `NativeBalanceDelta` records.
///
/// Filters deltas where `account_address` matches the given wallet:
/// - Fee-payer deltas produce `Fee` entries
/// - Other deltas produce `Transfer` entries
pub fn derive_ledger_from_native_balance_deltas(
    wallet: &str,
    user_id: Uuid,
    tx_id: Uuid,
    deltas: &[NativeBalanceDelta],
    entry_offset: &mut u32,
) -> Vec<LedgerEntry> {
    let wallet_lower = wallet.to_lowercase();
    let mut entries = Vec::new();

    for delta in deltas {
        if delta.account_address.to_lowercase() != wallet_lower {
            continue;
        }

        if delta.delta == BigDecimal::from(0) {
            continue;
        }

        let entry_type = if delta.is_fee_payer {
            EntryType::Fee
        } else {
            EntryType::Transfer
        };

        entries.push(LedgerEntry {
            id: deterministic_id(tx_id, *entry_offset),
            transaction_id: tx_id,
            user_id,
            wallet_address: wallet.to_string(),
            asset_symbol: delta.native_token.clone(),
            amount: delta.delta.clone(),
            entry_type,
            fiat_value: None,
        });
        *entry_offset += 1;
    }

    entries
}

/// Derive ledger entries from Silver `HlFillRecord` records.
///
/// Converts each fill into:
/// - A `Trade` entry with coin as asset_symbol and signed size as amount
/// - A `Fee` entry if fees are present and non-zero
/// - An `Income` entry if closed PnL is present and non-zero
///
/// Matches V1 `parse_fill` semantics.
pub fn derive_ledger_from_hl_fills(
    wallet: &str,
    user_id: Uuid,
    tx_id: Uuid,
    fills: &[HlFillRecord],
    entry_offset: &mut u32,
) -> Vec<LedgerEntry> {
    let mut entries = Vec::new();

    for fill in fills {
        let fiat_value = &fill.size * &fill.price;
        let signed_size = if fill.side == "B" {
            fill.size.clone()
        } else {
            -fill.size.clone()
        };

        entries.push(LedgerEntry {
            id: deterministic_id(tx_id, *entry_offset),
            transaction_id: tx_id,
            user_id,
            wallet_address: wallet.to_string(),
            asset_symbol: fill.coin.clone(),
            amount: signed_size,
            entry_type: EntryType::Trade,
            fiat_value: Some(fiat_value),
        });
        *entry_offset += 1;

        if let Some(ref fee) = fill.fee {
            if *fee != BigDecimal::from(0) {
                let fee_token = fill.fee_token.as_deref().unwrap_or("USDC");
                entries.push(LedgerEntry {
                    id: deterministic_id(tx_id, *entry_offset),
                    transaction_id: tx_id,
                    user_id,
                    wallet_address: wallet.to_string(),
                    asset_symbol: fee_token.to_string(),
                    amount: -fee.abs(),
                    entry_type: EntryType::Fee,
                    fiat_value: None,
                });
                *entry_offset += 1;
            }
        }

        if let Some(ref pnl) = fill.closed_pnl {
            if *pnl != BigDecimal::from(0) {
                entries.push(LedgerEntry {
                    id: deterministic_id(tx_id, *entry_offset),
                    transaction_id: tx_id,
                    user_id,
                    wallet_address: wallet.to_string(),
                    asset_symbol: "USDC".to_string(),
                    amount: pnl.clone(),
                    entry_type: EntryType::Income,
                    fiat_value: None,
                });
                *entry_offset += 1;
            }
        }
    }

    entries
}

/// Derive ledger entries from Silver `HlFundingPayment` records.
///
/// Converts each funding payment into a `Fee` entry, consistent with
/// V1 `parse_funding` behavior.
pub fn derive_ledger_from_hl_funding(
    wallet: &str,
    user_id: Uuid,
    tx_id: Uuid,
    payments: &[HlFundingPayment],
    entry_offset: &mut u32,
) -> Vec<LedgerEntry> {
    let mut entries = Vec::new();

    for payment in payments {
        entries.push(LedgerEntry {
            id: deterministic_id(tx_id, *entry_offset),
            transaction_id: tx_id,
            user_id,
            wallet_address: wallet.to_string(),
            asset_symbol: "USDC".to_string(),
            amount: payment.amount.clone(),
            entry_type: EntryType::Fee,
            fiat_value: None,
        });
        *entry_offset += 1;
    }

    entries
}

/// Orchestrate derivation of all ledger entries from Silver datasets.
///
/// Dispatches to chain-appropriate derivation functions based on the chain
/// family and merges results.
pub fn derive_all_ledger_entries(
    chain: Chain,
    wallet: &str,
    user_id: Uuid,
    tx_id: Uuid,
    records: &SilverRecords,
) -> Vec<LedgerEntry> {
    let family = ChainFamily::from(chain);
    let mut entry_offset: u32 = 0;
    let mut all_entries = Vec::new();

    match family {
        ChainFamily::Solana => {
            all_entries.extend(derive_ledger_from_native_balance_deltas(
                wallet,
                user_id,
                tx_id,
                &records.native_balance_deltas,
                &mut entry_offset,
            ));
            all_entries.extend(derive_ledger_from_token_transfers(
                wallet,
                user_id,
                tx_id,
                &records.token_transfers,
                &mut entry_offset,
            ));
        }
        ChainFamily::Evm => {
            all_entries.extend(derive_ledger_from_token_transfers(
                wallet,
                user_id,
                tx_id,
                &records.token_transfers,
                &mut entry_offset,
            ));
        }
        ChainFamily::Hyperliquid => {
            all_entries.extend(derive_ledger_from_hl_fills(
                wallet,
                user_id,
                tx_id,
                &records.hl_fills,
                &mut entry_offset,
            ));
            all_entries.extend(derive_ledger_from_hl_funding(
                wallet,
                user_id,
                tx_id,
                &records.hl_funding,
                &mut entry_offset,
            ));
        }
    }

    all_entries
}

// ---------------------------------------------------------------------------
// Derived Ledger Materializer implementations
// ---------------------------------------------------------------------------

/// V2 derived ledger materializer for Solana.
///
/// Produces `LedgerEntries` from Silver datasets (TokenTransfers,
/// NativeBalanceDeltas) rather than directly from Bronze raw_transactions.
pub struct SolanaDerivedLedgerMaterializer;

impl Materializer for SolanaDerivedLedgerMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::LedgerEntries
    }

    fn parser_version(&self) -> i32 {
        2
    }

    fn parser_hash(&self) -> &str {
        "sha256:solana_derived_ledger_v2_d7a1f3e9"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Solana
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Solana derived ledger entries: wallet-scoped entries from Silver token_transfers and native_balance_deltas"
                    .to_string(),
            source_bronze_tables: vec![
                "token_transfers".to_string(),
                "native_balance_deltas".to_string(),
            ],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// V2 derived ledger materializer for EVM.
///
/// Produces `LedgerEntries` from Silver TokenTransfers. Native ETH and gas
/// fee derivation requires EVM trace data not yet available in Silver.
pub struct EvmDerivedLedgerMaterializer;

impl Materializer for EvmDerivedLedgerMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::LedgerEntries
    }

    fn parser_version(&self) -> i32 {
        2
    }

    fn parser_hash(&self) -> &str {
        "sha256:evm_derived_ledger_v2_c3b5e8f2"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Evm
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "EVM derived ledger entries: wallet-scoped entries from Silver token_transfers"
                    .to_string(),
            source_bronze_tables: vec!["token_transfers".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// V2 derived ledger materializer for Hyperliquid.
///
/// Produces `LedgerEntries` from Silver HlFillRecord and HlFundingPayment
/// datasets.
pub struct HyperliquidDerivedLedgerMaterializer;

impl Materializer for HyperliquidDerivedLedgerMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::LedgerEntries
    }

    fn parser_version(&self) -> i32 {
        2
    }

    fn parser_hash(&self) -> &str {
        "sha256:hl_derived_ledger_v2_b9d4a7c1"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Hyperliquid
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Hyperliquid derived ledger entries: wallet-scoped entries from Silver hl_fills and hl_funding"
                    .to_string(),
            source_bronze_tables: vec!["hl_fills".to_string(), "hl_funding".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::str::FromStr;

    // -- Test helpers --

    fn make_token_transfer(from: &str, to: &str, symbol: &str, amount: &str) -> TokenTransfer {
        TokenTransfer {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "test-net".to_string(),
            token_address: format!("addr:{symbol}"),
            token_symbol: Some(symbol.to_string()),
            from_address: from.to_string(),
            to_address: to.to_string(),
            amount: BigDecimal::from_str(amount).unwrap(),
            decimals: 6,
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    fn make_native_delta(
        account: &str,
        token: &str,
        delta: &str,
        is_fee_payer: bool,
    ) -> NativeBalanceDelta {
        let delta_bd = BigDecimal::from_str(delta).unwrap();
        NativeBalanceDelta {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "test-net".to_string(),
            account_address: account.to_string(),
            native_token: token.to_string(),
            pre_balance: BigDecimal::from(100),
            post_balance: BigDecimal::from(100) + &delta_bd,
            delta: delta_bd,
            is_fee_payer,
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    fn make_hl_fill(
        coin: &str,
        side: &str,
        price: &str,
        size: &str,
        fee: Option<&str>,
        closed_pnl: Option<&str>,
    ) -> HlFillRecord {
        HlFillRecord {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "hypercore-mainnet".to_string(),
            coin: coin.to_string(),
            side: side.to_string(),
            price: BigDecimal::from_str(price).unwrap(),
            size: BigDecimal::from_str(size).unwrap(),
            direction: if side == "B" {
                Some("Open Long".to_string())
            } else {
                Some("Close Long".to_string())
            },
            closed_pnl: closed_pnl.map(|s| BigDecimal::from_str(s).unwrap()),
            fee: fee.map(|s| BigDecimal::from_str(s).unwrap()),
            fee_token: Some("USDC".to_string()),
            fill_time: 1700000000000,
            order_id: Some(12345),
            trade_id: Some(67890),
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    fn make_hl_funding(coin: &str, amount: &str) -> HlFundingPayment {
        HlFundingPayment {
            id: Uuid::new_v4(),
            raw_transaction_id: Some(Uuid::new_v4()),
            network: "hypercore-mainnet".to_string(),
            coin: coin.to_string(),
            amount: BigDecimal::from_str(amount).unwrap(),
            funding_rate: Some(BigDecimal::from_str("0.0001").unwrap()),
            payment_time: 1700000000000,
            dataset_version_id: None,
            created_at: Utc::now(),
        }
    }

    // -- (a) derivation from token transfers produces correct Transfer entries --

    #[test]
    fn derive_token_transfer_incoming() {
        let wallet = "0xWallet1";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let transfers = vec![make_token_transfer("0xOther", "0xWallet1", "USDC", "100.5")];

        let mut offset = 0;
        let entries =
            derive_ledger_from_token_transfers(wallet, user_id, tx_id, &transfers, &mut offset);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "USDC");
        assert_eq!(entries[0].amount, BigDecimal::from_str("100.5").unwrap());
        assert!(matches!(entries[0].entry_type, EntryType::Transfer));
        assert_eq!(entries[0].wallet_address, wallet);
    }

    #[test]
    fn derive_token_transfer_outgoing() {
        let wallet = "0xWallet1";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let transfers = vec![make_token_transfer("0xWallet1", "0xOther", "USDT", "50.0")];

        let mut offset = 0;
        let entries =
            derive_ledger_from_token_transfers(wallet, user_id, tx_id, &transfers, &mut offset);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "USDT");
        assert_eq!(entries[0].amount, BigDecimal::from_str("-50.0").unwrap());
        assert!(matches!(entries[0].entry_type, EntryType::Transfer));
    }

    #[test]
    fn derive_token_transfer_self_transfer() {
        let wallet = "0xWallet1";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let transfers = vec![make_token_transfer(
            "0xWallet1",
            "0xWallet1",
            "USDC",
            "10.0",
        )];

        let mut offset = 0;
        let entries =
            derive_ledger_from_token_transfers(wallet, user_id, tx_id, &transfers, &mut offset);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].amount, BigDecimal::from_str("-10.0").unwrap());
        assert_eq!(entries[1].amount, BigDecimal::from_str("10.0").unwrap());
    }

    // -- (b) derivation from native balance deltas produces correct Fee/Transfer entries --

    #[test]
    fn derive_native_delta_fee_payer() {
        let wallet = "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let deltas = vec![make_native_delta(wallet, "SOL", "-0.500005", true)];

        let mut offset = 0;
        let entries =
            derive_ledger_from_native_balance_deltas(wallet, user_id, tx_id, &deltas, &mut offset);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "SOL");
        assert_eq!(
            entries[0].amount,
            BigDecimal::from_str("-0.500005").unwrap()
        );
        assert!(matches!(entries[0].entry_type, EntryType::Fee));
    }

    #[test]
    fn derive_native_delta_non_fee_payer() {
        let wallet = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let deltas = vec![make_native_delta(wallet, "SOL", "0.5", false)];

        let mut offset = 0;
        let entries =
            derive_ledger_from_native_balance_deltas(wallet, user_id, tx_id, &deltas, &mut offset);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "SOL");
        assert_eq!(entries[0].amount, BigDecimal::from_str("0.5").unwrap());
        assert!(matches!(entries[0].entry_type, EntryType::Transfer));
    }

    #[test]
    fn derive_native_delta_zero_skipped() {
        let wallet = "TestWallet";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let deltas = vec![make_native_delta(wallet, "SOL", "0", false)];

        let mut offset = 0;
        let entries =
            derive_ledger_from_native_balance_deltas(wallet, user_id, tx_id, &deltas, &mut offset);

        assert_eq!(entries.len(), 0, "zero deltas should be skipped");
    }

    // -- (c) derivation from HL fills produces correct Trade entries --

    #[test]
    fn derive_hl_fill_buy() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let fills = vec![make_hl_fill(
            "ETH",
            "B",
            "3500.0",
            "2.0",
            Some("3.50"),
            Some("0.0"),
        )];

        let mut offset = 0;
        let entries = derive_ledger_from_hl_fills(wallet, user_id, tx_id, &fills, &mut offset);

        // Trade + Fee (closedPnl=0 → no income)
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
    fn derive_hl_fill_sell_with_pnl() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let fills = vec![make_hl_fill(
            "BTC",
            "A",
            "42000.0",
            "0.5",
            Some("10.50"),
            Some("250.0"),
        )];

        let mut offset = 0;
        let entries = derive_ledger_from_hl_fills(wallet, user_id, tx_id, &fills, &mut offset);

        // Trade + Fee + Income
        assert_eq!(entries.len(), 3);

        let trade = &entries[0];
        assert_eq!(trade.asset_symbol, "BTC");
        assert_eq!(trade.amount, BigDecimal::from_str("-0.5").unwrap());
        assert!(matches!(trade.entry_type, EntryType::Trade));

        let fee = &entries[1];
        assert_eq!(fee.amount, BigDecimal::from_str("-10.50").unwrap());
        assert!(matches!(fee.entry_type, EntryType::Fee));

        let income = &entries[2];
        assert_eq!(income.asset_symbol, "USDC");
        assert_eq!(income.amount, BigDecimal::from_str("250.0").unwrap());
        assert!(matches!(income.entry_type, EntryType::Income));
    }

    #[test]
    fn derive_hl_fill_no_fee_no_pnl() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let fills = vec![make_hl_fill("SOL", "B", "150.0", "10.0", None, None)];

        let mut offset = 0;
        let entries = derive_ledger_from_hl_fills(wallet, user_id, tx_id, &fills, &mut offset);

        assert_eq!(entries.len(), 1, "only Trade, no Fee or Income");
        assert!(matches!(entries[0].entry_type, EntryType::Trade));
        assert_eq!(entries[0].asset_symbol, "SOL");
    }

    // -- (d) derivation from HL funding produces correct Fee entries --

    #[test]
    fn derive_hl_funding_positive() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let payments = vec![make_hl_funding("ETH", "5.0")];

        let mut offset = 0;
        let entries = derive_ledger_from_hl_funding(wallet, user_id, tx_id, &payments, &mut offset);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "USDC");
        assert_eq!(entries[0].amount, BigDecimal::from_str("5.0").unwrap());
        assert!(matches!(entries[0].entry_type, EntryType::Fee));
    }

    #[test]
    fn derive_hl_funding_negative() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let payments = vec![make_hl_funding("BTC", "-2.50")];

        let mut offset = 0;
        let entries = derive_ledger_from_hl_funding(wallet, user_id, tx_id, &payments, &mut offset);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].amount, BigDecimal::from_str("-2.50").unwrap());
        assert!(matches!(entries[0].entry_type, EntryType::Fee));
    }

    // -- (e) wallet filtering works correctly --

    #[test]
    fn wallet_filtering_token_transfers() {
        let wallet = "0xMyWallet";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let transfers = vec![
            make_token_transfer("0xOther1", "0xOther2", "USDC", "100.0"),
            make_token_transfer("0xMyWallet", "0xOther3", "USDT", "50.0"),
            make_token_transfer("0xOther4", "0xMyWallet", "ETH", "1.0"),
            make_token_transfer("0xOther5", "0xOther6", "DAI", "200.0"),
        ];

        let mut offset = 0;
        let entries =
            derive_ledger_from_token_transfers(wallet, user_id, tx_id, &transfers, &mut offset);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].asset_symbol, "USDT");
        assert_eq!(entries[0].amount, BigDecimal::from_str("-50.0").unwrap());
        assert_eq!(entries[1].asset_symbol, "ETH");
        assert_eq!(entries[1].amount, BigDecimal::from_str("1.0").unwrap());
    }

    #[test]
    fn wallet_filtering_case_insensitive() {
        let wallet = "0xMyWallet";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let transfers = vec![make_token_transfer(
            "0xOther",
            "0xmywallet",
            "USDC",
            "100.0",
        )];

        let mut offset = 0;
        let entries =
            derive_ledger_from_token_transfers(wallet, user_id, tx_id, &transfers, &mut offset);

        assert_eq!(entries.len(), 1, "case-insensitive matching should work");
    }

    #[test]
    fn wallet_filtering_native_deltas() {
        let wallet = "MyAccount";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let deltas = vec![
            make_native_delta("OtherAccount1", "SOL", "-0.5", true),
            make_native_delta("MyAccount", "SOL", "0.5", false),
            make_native_delta("OtherAccount2", "SOL", "-0.000005", false),
        ];

        let mut offset = 0;
        let entries =
            derive_ledger_from_native_balance_deltas(wallet, user_id, tx_id, &deltas, &mut offset);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].amount, BigDecimal::from_str("0.5").unwrap());
    }

    // -- (f) deterministic IDs are stable across re-derivation --

    #[test]
    fn deterministic_ids_stable_token_transfers() {
        let wallet = "0xWallet";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let transfers = vec![make_token_transfer("0xOther", "0xWallet", "USDC", "100.0")];

        let mut offset1 = 0;
        let entries1 =
            derive_ledger_from_token_transfers(wallet, user_id, tx_id, &transfers, &mut offset1);

        let mut offset2 = 0;
        let entries2 =
            derive_ledger_from_token_transfers(wallet, user_id, tx_id, &transfers, &mut offset2);

        assert_eq!(entries1.len(), entries2.len());
        for (e1, e2) in entries1.iter().zip(entries2.iter()) {
            assert_eq!(e1.id, e2.id, "IDs must be stable across re-derivation");
        }
    }

    #[test]
    fn deterministic_ids_stable_hl_fills() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let fills = vec![make_hl_fill(
            "ETH",
            "B",
            "3500.0",
            "2.0",
            Some("3.50"),
            Some("100.0"),
        )];

        let mut offset1 = 0;
        let entries1 = derive_ledger_from_hl_fills(wallet, user_id, tx_id, &fills, &mut offset1);

        let mut offset2 = 0;
        let entries2 = derive_ledger_from_hl_fills(wallet, user_id, tx_id, &fills, &mut offset2);

        assert_eq!(entries1.len(), entries2.len());
        for (e1, e2) in entries1.iter().zip(entries2.iter()) {
            assert_eq!(e1.id, e2.id, "IDs must be stable across re-derivation");
        }
    }

    #[test]
    fn deterministic_ids_stable_native_deltas() {
        let wallet = "TestWallet";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let deltas = vec![make_native_delta(wallet, "SOL", "-0.5", true)];

        let mut offset1 = 0;
        let entries1 =
            derive_ledger_from_native_balance_deltas(wallet, user_id, tx_id, &deltas, &mut offset1);

        let mut offset2 = 0;
        let entries2 =
            derive_ledger_from_native_balance_deltas(wallet, user_id, tx_id, &deltas, &mut offset2);

        assert_eq!(entries1.len(), entries2.len());
        for (e1, e2) in entries1.iter().zip(entries2.iter()) {
            assert_eq!(e1.id, e2.id, "IDs must be stable across re-derivation");
        }
    }

    #[test]
    fn deterministic_ids_stable_hl_funding() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let payments = vec![make_hl_funding("ETH", "-2.50")];

        let mut offset1 = 0;
        let entries1 =
            derive_ledger_from_hl_funding(wallet, user_id, tx_id, &payments, &mut offset1);

        let mut offset2 = 0;
        let entries2 =
            derive_ledger_from_hl_funding(wallet, user_id, tx_id, &payments, &mut offset2);

        assert_eq!(entries1.len(), entries2.len());
        for (e1, e2) in entries1.iter().zip(entries2.iter()) {
            assert_eq!(e1.id, e2.id, "IDs must be stable across re-derivation");
        }
    }

    // -- (g) DerivedLedgerMaterializer trait implementations --

    #[test]
    fn derived_materializers_produce_ledger_entries() {
        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(SolanaDerivedLedgerMaterializer),
            Box::new(EvmDerivedLedgerMaterializer),
            Box::new(HyperliquidDerivedLedgerMaterializer),
        ];

        for m in &materializers {
            assert_eq!(
                m.dataset_name(),
                DatasetName::LedgerEntries,
                "derived materializer for {:?} must produce ledger_entries",
                m.chain_family()
            );
        }
    }

    #[test]
    fn derived_materializers_are_v2() {
        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(SolanaDerivedLedgerMaterializer),
            Box::new(EvmDerivedLedgerMaterializer),
            Box::new(HyperliquidDerivedLedgerMaterializer),
        ];

        for m in &materializers {
            assert_eq!(
                m.parser_version(),
                2,
                "derived materializer for {:?} must be parser_version 2",
                m.chain_family()
            );
        }
    }

    #[test]
    fn derived_materializers_have_distinct_hashes() {
        use std::collections::HashSet;

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(SolanaDerivedLedgerMaterializer),
            Box::new(EvmDerivedLedgerMaterializer),
            Box::new(HyperliquidDerivedLedgerMaterializer),
        ];

        let hashes: HashSet<&str> = materializers.iter().map(|m| m.parser_hash()).collect();
        assert_eq!(
            hashes.len(),
            3,
            "all 3 derived materializers must have distinct hashes"
        );
    }

    #[test]
    fn derived_materializers_distinct_from_all_13_existing() {
        use std::collections::HashSet;

        let existing_hashes: HashSet<&str> = [
            "sha256:solana_ledger_v1_a3f8c9d2",
            "sha256:evm_ledger_v1_b7e4d1f5",
            "sha256:hl_ledger_v1_c5a2e8b3",
            "sha256:solana_token_transfers_v1_d4e9f1a7",
            "sha256:evm_token_transfers_v1_a1c5e7b9",
            "sha256:hl_token_transfers_v1_e7f2a4d8",
            "sha256:solana_native_deltas_v1_b2c3a8f6",
            "sha256:hl_native_deltas_v1_f3d6c9a2",
            "sha256:evm_decoded_events_v1_c8d4f2a6",
            "sha256:solana_decoded_events_v1_e5b7c1d9",
            "sha256:hl_fill_records_v1_a1b2c3d4",
            "sha256:hl_funding_payments_v1_e5f6g7h8",
            "sha256:hl_position_changes_v1_i9j0k1l2",
        ]
        .into_iter()
        .collect();
        assert_eq!(existing_hashes.len(), 13, "sanity: 13 existing hashes");

        let derived_materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(SolanaDerivedLedgerMaterializer),
            Box::new(EvmDerivedLedgerMaterializer),
            Box::new(HyperliquidDerivedLedgerMaterializer),
        ];

        for m in &derived_materializers {
            assert!(
                !existing_hashes.contains(m.parser_hash()),
                "derived materializer {:?} hash {} must not collide with existing materializers",
                m.chain_family(),
                m.parser_hash()
            );
        }
    }

    #[test]
    fn derived_materializer_descriptors_are_valid() {
        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(SolanaDerivedLedgerMaterializer),
            Box::new(EvmDerivedLedgerMaterializer),
            Box::new(HyperliquidDerivedLedgerMaterializer),
        ];

        for m in &materializers {
            let desc = m.descriptor();
            assert!(
                desc.validate().is_ok(),
                "descriptor for derived {:?} should be valid",
                m.chain_family()
            );
            assert_eq!(desc.name, DatasetName::LedgerEntries);
        }
    }

    // -- derive_all_ledger_entries orchestration tests --

    #[test]
    fn derive_all_solana() {
        let wallet = "SolWallet";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let records = SilverRecords {
            native_balance_deltas: vec![make_native_delta("SolWallet", "SOL", "-0.5", true)],
            token_transfers: vec![make_token_transfer(
                "OtherWallet",
                "SolWallet",
                "USDC",
                "100.0",
            )],
            ..SilverRecords::default()
        };

        let entries = derive_all_ledger_entries(Chain::Solana, wallet, user_id, tx_id, &records);

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].asset_symbol, "SOL");
        assert!(matches!(entries[0].entry_type, EntryType::Fee));
        assert_eq!(entries[1].asset_symbol, "USDC");
        assert!(matches!(entries[1].entry_type, EntryType::Transfer));
    }

    #[test]
    fn derive_all_evm() {
        let wallet = "0xMyWallet";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let records = SilverRecords {
            token_transfers: vec![make_token_transfer(
                "0xSender",
                "0xMyWallet",
                "USDC",
                "500.0",
            )],
            ..SilverRecords::default()
        };

        let entries = derive_all_ledger_entries(Chain::Ethereum, wallet, user_id, tx_id, &records);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].asset_symbol, "USDC");
        assert_eq!(entries[0].amount, BigDecimal::from_str("500.0").unwrap());
    }

    #[test]
    fn derive_all_hyperliquid() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let records = SilverRecords {
            hl_fills: vec![make_hl_fill(
                "ETH",
                "B",
                "3500.0",
                "2.0",
                Some("3.50"),
                None,
            )],
            hl_funding: vec![make_hl_funding("BTC", "-1.25")],
            ..SilverRecords::default()
        };

        let entries =
            derive_all_ledger_entries(Chain::Hyperliquid, wallet, user_id, tx_id, &records);

        // Fill: Trade + Fee. Funding: Fee.
        assert_eq!(entries.len(), 3);
        assert!(matches!(entries[0].entry_type, EntryType::Trade));
        assert!(matches!(entries[1].entry_type, EntryType::Fee));
        assert!(matches!(entries[2].entry_type, EntryType::Fee));
    }

    #[test]
    fn derive_all_empty_records() {
        let wallet = "0xtest";
        let user_id = Uuid::nil();
        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();

        let records = SilverRecords::default();

        let entries = derive_all_ledger_entries(Chain::Solana, wallet, user_id, tx_id, &records);
        assert!(entries.is_empty());

        let entries = derive_all_ledger_entries(Chain::Ethereum, wallet, user_id, tx_id, &records);
        assert!(entries.is_empty());

        let entries =
            derive_all_ledger_entries(Chain::Hyperliquid, wallet, user_id, tx_id, &records);
        assert!(entries.is_empty());
    }

    // -- Equivalence tests (step 9) --
    // Run both V1 direct parsing and V2 Silver derivation on the same synthetic
    // data and compare the resulting LedgerEntry sets for semantic equivalence.

    #[test]
    fn equivalence_hl_fill_v1_vs_v2() {
        use crate::hyperliquid_parser::{extract_hl_fill_records, parse_hyperliquid_transaction};

        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let user_id = Uuid::nil();
        let wallet = "0xtest";

        let fill_data = serde_json::json!({
            "coin": "ETH",
            "px": "3500.0",
            "sz": "2.0",
            "side": "B",
            "time": 1700000000000u64,
            "hash": "0xfill1",
            "fee": "3.50",
            "feeToken": "USDC",
            "closedPnl": "0.0"
        });

        let raw_metadata = serde_json::json!({ "type": "fill", "data": fill_data });

        // V1: direct parse
        let v1_tx = spectraplex_core::models::Transaction {
            id: tx_id,
            user_id,
            wallet_address: wallet.to_string(),
            timestamp: 1700000000,
            tx_hash: "0xhash".to_string(),
            chain: Chain::Hyperliquid,
            raw_metadata: raw_metadata.clone(),
        };
        let v1_entries = parse_hyperliquid_transaction(&v1_tx).unwrap();

        // V2: extract Silver, then derive
        let hl_fills = extract_hl_fill_records(Some(tx_id), "hypercore-mainnet", &raw_metadata);
        let mut offset = 0;
        let v2_entries =
            derive_ledger_from_hl_fills(wallet, user_id, tx_id, &hl_fills, &mut offset);

        assert_eq!(v1_entries.len(), v2_entries.len(), "same number of entries");
        for (v1, v2) in v1_entries.iter().zip(v2_entries.iter()) {
            assert_eq!(
                std::mem::discriminant(&v1.entry_type),
                std::mem::discriminant(&v2.entry_type),
                "same entry_type"
            );
            assert_eq!(v1.asset_symbol, v2.asset_symbol, "same asset_symbol");
            assert_eq!(v1.amount, v2.amount, "same amount");
            assert_eq!(v1.fiat_value, v2.fiat_value, "same fiat_value");
        }
    }

    #[test]
    fn equivalence_hl_fill_sell_with_pnl_v1_vs_v2() {
        use crate::hyperliquid_parser::{extract_hl_fill_records, parse_hyperliquid_transaction};

        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001").unwrap();
        let user_id = Uuid::nil();
        let wallet = "0xtest";

        let fill_data = serde_json::json!({
            "coin": "BTC",
            "px": "42000.0",
            "sz": "0.5",
            "side": "A",
            "time": 1700000000000u64,
            "hash": "0xfill2",
            "fee": "10.50",
            "feeToken": "USDC",
            "closedPnl": "250.0"
        });

        let raw_metadata = serde_json::json!({ "type": "fill", "data": fill_data });

        let v1_tx = spectraplex_core::models::Transaction {
            id: tx_id,
            user_id,
            wallet_address: wallet.to_string(),
            timestamp: 1700000000,
            tx_hash: "0xhash".to_string(),
            chain: Chain::Hyperliquid,
            raw_metadata: raw_metadata.clone(),
        };
        let v1_entries = parse_hyperliquid_transaction(&v1_tx).unwrap();

        let hl_fills = extract_hl_fill_records(Some(tx_id), "hypercore-mainnet", &raw_metadata);
        let mut offset = 0;
        let v2_entries =
            derive_ledger_from_hl_fills(wallet, user_id, tx_id, &hl_fills, &mut offset);

        assert_eq!(
            v1_entries.len(),
            v2_entries.len(),
            "same number of entries (Trade + Fee + Income)"
        );
        for (v1, v2) in v1_entries.iter().zip(v2_entries.iter()) {
            assert_eq!(
                std::mem::discriminant(&v1.entry_type),
                std::mem::discriminant(&v2.entry_type),
                "same entry_type"
            );
            assert_eq!(v1.asset_symbol, v2.asset_symbol, "same asset_symbol");
            assert_eq!(v1.amount, v2.amount, "same amount");
        }
    }

    #[test]
    fn equivalence_hl_funding_v1_vs_v2() {
        use crate::hyperliquid_parser::{
            extract_hl_funding_payments, parse_hyperliquid_transaction,
        };

        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let user_id = Uuid::nil();
        let wallet = "0xtest";

        let funding_data = serde_json::json!({
            "coin": "ETH",
            "usdc": "-2.50",
            "fundingRate": "0.0001",
            "time": 1700000000000u64,
            "hash": "0xfunding1"
        });

        let raw_metadata = serde_json::json!({ "type": "funding", "data": funding_data });

        // V1
        let v1_tx = spectraplex_core::models::Transaction {
            id: tx_id,
            user_id,
            wallet_address: wallet.to_string(),
            timestamp: 1700000000,
            tx_hash: "0xhash".to_string(),
            chain: Chain::Hyperliquid,
            raw_metadata: raw_metadata.clone(),
        };
        let v1_entries = parse_hyperliquid_transaction(&v1_tx).unwrap();

        // V2
        let hl_funding =
            extract_hl_funding_payments(Some(tx_id), "hypercore-mainnet", &raw_metadata);
        let mut offset = 0;
        let v2_entries =
            derive_ledger_from_hl_funding(wallet, user_id, tx_id, &hl_funding, &mut offset);

        assert_eq!(v1_entries.len(), v2_entries.len(), "same number of entries");
        for (v1, v2) in v1_entries.iter().zip(v2_entries.iter()) {
            assert_eq!(
                std::mem::discriminant(&v1.entry_type),
                std::mem::discriminant(&v2.entry_type),
                "same entry_type"
            );
            assert_eq!(v1.asset_symbol, v2.asset_symbol, "same asset_symbol");
            assert_eq!(v1.amount, v2.amount, "same amount");
        }
    }

    #[test]
    fn equivalence_evm_token_transfer_v1_vs_v2() {
        use crate::evm_parser::{extract_evm_token_transfers, parse_evm_transaction};

        let tx_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let user_id = Uuid::nil();
        let wallet = "0xabcdef1234567890abcdef1234567890abcdef12";

        // ERC-20 Transfer: from some address TO our wallet (incoming USDC)
        let from_padded = format!(
            "0x000000000000000000000000{}",
            "1111111111111111111111111111111111111111"
        );
        let to_padded = format!("0x000000000000000000000000{}", &wallet[2..]);

        let raw_metadata = serde_json::json!({
            "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                from_padded,
                to_padded,
            ],
            // 10_000_000 raw units; USDC has 6 decimals → 10.0 USDC
            "data": "0x0000000000000000000000000000000000000000000000000000000000989680",
            "address": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
        });

        // V1
        let v1_tx = spectraplex_core::models::Transaction {
            id: tx_id,
            user_id,
            wallet_address: wallet.to_string(),
            timestamp: 1700000000,
            tx_hash: "0xhash".to_string(),
            chain: Chain::Ethereum,
            raw_metadata: raw_metadata.clone(),
        };
        let v1_entries = parse_evm_transaction(&v1_tx).unwrap();

        // V2: extract Silver, then derive
        let token_transfers =
            extract_evm_token_transfers(Some(tx_id), "ethereum-mainnet", &raw_metadata);
        let mut offset = 0;
        let v2_entries = derive_ledger_from_token_transfers(
            wallet,
            user_id,
            tx_id,
            &token_transfers,
            &mut offset,
        );

        // Both should produce one incoming transfer
        assert_eq!(
            v1_entries.len(),
            v2_entries.len(),
            "same number of token transfer entries"
        );
        for (v1, v2) in v1_entries.iter().zip(v2_entries.iter()) {
            assert_eq!(
                std::mem::discriminant(&v1.entry_type),
                std::mem::discriminant(&v2.entry_type),
                "same entry_type"
            );
            assert_eq!(v1.asset_symbol, v2.asset_symbol, "same asset_symbol");
            assert_eq!(v1.amount, v2.amount, "same amount");
        }
    }
}
