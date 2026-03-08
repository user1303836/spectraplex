use bigdecimal::BigDecimal;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionStatusMeta,
};
use spectraplex_core::models::{EntryType, LedgerEntry, Transaction};
use std::str::FromStr;

use crate::deterministic_id;

pub fn parse_solana_transaction(tx: &Transaction) -> anyhow::Result<Vec<LedgerEntry>> {
    let mut entries = Vec::new();
    let mut entry_index: u32 = 0;

    let sol_tx: EncodedConfirmedTransactionWithStatusMeta =
        serde_json::from_value(tx.raw_metadata.clone())?;

    let meta = match &sol_tx.transaction.meta {
        Some(m) => m,
        None => return Ok(vec![]),
    };

    // Skip failed transactions
    if meta.err.is_some() {
        return Ok(vec![]);
    }

    let transaction = &sol_tx.transaction.transaction;
    if let solana_transaction_status::EncodedTransaction::Json(ui_tx) = transaction {
        if let solana_transaction_status::UiMessage::Parsed(message) = &ui_tx.message {
            if let Some(idx) = message
                .account_keys
                .iter()
                .position(|k| k.pubkey == tx.wallet_address)
            {
                let lamport_change = extract_sol_change_lamports(meta, idx);
                let fee_lamports = meta.fee;
                let is_fee_payer = idx == 0;

                // If this wallet is the fee payer, separate fee from the net transfer
                if is_fee_payer && fee_lamports > 0 {
                    // Fee entry (always negative)
                    let fee_amount = lamports_to_sol(-(fee_lamports as i128));
                    entries.push(LedgerEntry {
                        id: deterministic_id(tx.id, entry_index),
                        transaction_id: tx.id,
                        user_id: tx.user_id,
                        wallet_address: tx.wallet_address.clone(),
                        asset_symbol: "SOL".to_string(),
                        amount: fee_amount,
                        entry_type: EntryType::Fee,
                        fiat_value: None,
                    });
                    entry_index += 1;

                    // Net transfer amount = total balance change + fee (since fee is included in the balance change)
                    let transfer_lamports = lamport_change + fee_lamports as i128;
                    if transfer_lamports != 0 {
                        let transfer_amount = lamports_to_sol(transfer_lamports);
                        entries.push(LedgerEntry {
                            id: deterministic_id(tx.id, entry_index),
                            transaction_id: tx.id,
                            user_id: tx.user_id,
                            wallet_address: tx.wallet_address.clone(),
                            asset_symbol: "SOL".to_string(),
                            amount: transfer_amount,
                            entry_type: EntryType::Transfer,
                            fiat_value: None,
                        });
                        entry_index += 1;
                    }
                } else if lamport_change != 0 {
                    // Non-fee-payer: entire balance change is a transfer
                    let amount = lamports_to_sol(lamport_change);
                    entries.push(LedgerEntry {
                        id: deterministic_id(tx.id, entry_index),
                        transaction_id: tx.id,
                        user_id: tx.user_id,
                        wallet_address: tx.wallet_address.clone(),
                        asset_symbol: "SOL".to_string(),
                        amount,
                        entry_type: EntryType::Transfer,
                        fiat_value: None,
                    });
                    entry_index += 1;
                }
            }
        }
    }

    // Extract SPL Token Changes
    if let OptionSerializer::Some(pre_token_balances) = &meta.pre_token_balances {
        if let OptionSerializer::Some(post_token_balances) = &meta.post_token_balances {
            for post in post_token_balances {
                let owner_match = match &post.owner {
                    OptionSerializer::Some(owner) => owner == &tx.wallet_address,
                    OptionSerializer::None => false,
                    OptionSerializer::Skip => false,
                };

                if owner_match {
                    let mint = post.mint.clone();
                    let decimals = post.ui_token_amount.decimals as u32;

                    let pre_raw = pre_token_balances
                        .iter()
                        .find(|p| p.account_index == post.account_index)
                        .and_then(|p| p.ui_token_amount.amount.parse::<i128>().ok())
                        .unwrap_or(0);

                    let post_raw = post.ui_token_amount.amount.parse::<i128>().unwrap_or(0);
                    let delta_raw = post_raw - pre_raw;

                    if delta_raw != 0 {
                        let amount = token_raw_to_decimal(delta_raw, decimals);
                        let symbol = spl_token_symbol(&mint);
                        entries.push(LedgerEntry {
                            id: deterministic_id(tx.id, entry_index),
                            transaction_id: tx.id,
                            user_id: tx.user_id,
                            wallet_address: tx.wallet_address.clone(),
                            asset_symbol: symbol,
                            amount,
                            entry_type: EntryType::Transfer,
                            fiat_value: None,
                        });
                        entry_index += 1;
                    }
                }
            }
        }
    }

    let _ = entry_index;
    Ok(entries)
}

/// Extract the raw lamport balance change for a given account index.
/// Uses i128 to avoid truncation when u64 values exceed i64::MAX.
fn extract_sol_change_lamports(meta: &UiTransactionStatusMeta, wallet_index: usize) -> i128 {
    let pre = meta.pre_balances.get(wallet_index).copied().unwrap_or(0) as i128;
    let post = meta.post_balances.get(wallet_index).copied().unwrap_or(0) as i128;
    post - pre
}

/// Convert lamports (i128) to SOL as BigDecimal without floating-point precision loss.
fn lamports_to_sol(lamports: i128) -> BigDecimal {
    let raw = BigDecimal::from_str(&format!("{}", lamports)).unwrap();
    let divisor = BigDecimal::from_str("1000000000").unwrap();
    raw / divisor
}

/// Convert raw token amount to BigDecimal using the token's decimals.
fn token_raw_to_decimal(raw: i128, decimals: u32) -> BigDecimal {
    let raw_bd = BigDecimal::from_str(&format!("{}", raw)).unwrap();
    let divisor = BigDecimal::from_str(&format!("1{}", "0".repeat(decimals as usize))).unwrap();
    raw_bd / divisor
}

/// Lookup a human-readable symbol for well-known SPL tokens.
/// Falls back to the mint address for unknown tokens.
fn spl_token_symbol(mint: &str) -> String {
    match mint {
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => "USDC".to_string(),
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => "USDT".to_string(),
        "So11111111111111111111111111111111111111112" => "SOL".to_string(),
        "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So" => "mSOL".to_string(),
        "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs" => "WETH".to_string(),
        "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263" => "BONK".to_string(),
        "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN" => "JUP".to_string(),
        "7dHbWXmci3dT8UFYWYZweBLXgycu7Y3iL6trKn1Y7ARj" => "stSOL".to_string(),
        "RLBxxFkseAZ4RgJH3Sqn8jXxhmGoz9jWxDNJMh8pL7a" => "RLSOL".to_string(),
        "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn" => "jitoSOL".to_string(),
        _ => mint.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Token Transfer extraction (P3-W2)
// ---------------------------------------------------------------------------

use chrono::Utc;
use spectraplex_core::materializer::{
    DatasetDescriptor, DatasetName, Materializer, NativeBalanceDelta, TokenTransfer,
};
use spectraplex_core::v2::ChainFamily;
use uuid::Uuid;

/// Extract SPL token transfers from a Solana raw transaction.
///
/// Uses pre/post token balance arrays from the transaction metadata to compute
/// deltas for each token account owner. Emits one `TokenTransfer` per non-zero
/// delta, with the owner as either from_address or to_address depending on sign.
pub fn extract_solana_token_transfers(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<TokenTransfer> {
    let sol_tx: EncodedConfirmedTransactionWithStatusMeta =
        match serde_json::from_value(raw_metadata.clone()) {
            Ok(tx) => tx,
            Err(_) => return vec![],
        };

    let meta = match &sol_tx.transaction.meta {
        Some(m) => m,
        None => return vec![],
    };

    if meta.err.is_some() {
        return vec![];
    }

    let mut transfers = Vec::new();

    if let OptionSerializer::Some(pre_token_balances) = &meta.pre_token_balances {
        if let OptionSerializer::Some(post_token_balances) = &meta.post_token_balances {
            for post in post_token_balances {
                let owner = match &post.owner {
                    OptionSerializer::Some(o) => o.clone(),
                    _ => continue,
                };

                let mint = post.mint.clone();
                let decimals = post.ui_token_amount.decimals as u32;

                let pre_raw = pre_token_balances
                    .iter()
                    .find(|p| p.account_index == post.account_index)
                    .and_then(|p| p.ui_token_amount.amount.parse::<i128>().ok())
                    .unwrap_or(0);

                let post_raw = post.ui_token_amount.amount.parse::<i128>().unwrap_or(0);
                let delta_raw = post_raw - pre_raw;

                if delta_raw == 0 {
                    continue;
                }

                let amount = token_raw_to_decimal(delta_raw.unsigned_abs() as i128, decimals);
                let symbol = spl_token_symbol(&mint);

                let (from, to) = if delta_raw > 0 {
                    // Incoming: unknown sender -> owner
                    ("unknown".to_string(), owner)
                } else {
                    // Outgoing: owner -> unknown receiver
                    (owner, "unknown".to_string())
                };

                transfers.push(TokenTransfer {
                    id: Uuid::new_v4(),
                    raw_transaction_id: raw_tx_id,
                    network: network.to_string(),
                    token_address: mint,
                    token_symbol: Some(symbol),
                    from_address: from,
                    to_address: to,
                    amount,
                    decimals: decimals as i32,
                    dataset_version_id: None,
                    created_at: Utc::now(),
                });
            }
        }
    }

    transfers
}

// ---------------------------------------------------------------------------
// Native Balance Delta extraction (P3-W2)
// ---------------------------------------------------------------------------

/// Extract SOL native balance deltas from a Solana raw transaction.
///
/// Uses pre/post balance arrays and the account keys from the transaction.
/// Flags the first signer (index 0) as the fee payer.
pub fn extract_solana_native_balance_deltas(
    raw_tx_id: Option<Uuid>,
    network: &str,
    raw_metadata: &serde_json::Value,
) -> Vec<NativeBalanceDelta> {
    let sol_tx: EncodedConfirmedTransactionWithStatusMeta =
        match serde_json::from_value(raw_metadata.clone()) {
            Ok(tx) => tx,
            Err(_) => return vec![],
        };

    let meta = match &sol_tx.transaction.meta {
        Some(m) => m,
        None => return vec![],
    };

    if meta.err.is_some() {
        return vec![];
    }

    let account_keys: Vec<String> =
        if let solana_transaction_status::EncodedTransaction::Json(ui_tx) =
            &sol_tx.transaction.transaction
        {
            match &ui_tx.message {
                solana_transaction_status::UiMessage::Parsed(msg) => {
                    msg.account_keys.iter().map(|k| k.pubkey.clone()).collect()
                }
                solana_transaction_status::UiMessage::Raw(msg) => msg.account_keys.clone(),
            }
        } else {
            return vec![];
        };

    let mut deltas = Vec::new();

    for (idx, account) in account_keys.iter().enumerate() {
        let pre = meta.pre_balances.get(idx).copied().unwrap_or(0) as i128;
        let post = meta.post_balances.get(idx).copied().unwrap_or(0) as i128;
        let change = post - pre;

        if change == 0 {
            continue;
        }

        let pre_sol = lamports_to_sol(pre);
        let post_sol = lamports_to_sol(post);
        let delta_sol = lamports_to_sol(change);

        deltas.push(NativeBalanceDelta {
            id: Uuid::new_v4(),
            raw_transaction_id: raw_tx_id,
            network: network.to_string(),
            account_address: account.clone(),
            native_token: "SOL".to_string(),
            pre_balance: pre_sol,
            post_balance: post_sol,
            delta: delta_sol,
            is_fee_payer: idx == 0,
            dataset_version_id: None,
            created_at: Utc::now(),
        });
    }

    deltas
}

// ---------------------------------------------------------------------------
// Materializer implementations
// ---------------------------------------------------------------------------

/// Materializer wrapper for the Solana ledger parser.
pub struct SolanaLedgerMaterializer;

impl Materializer for SolanaLedgerMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::LedgerEntries
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:solana_ledger_v1_a3f8c9d2"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Solana
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Solana ledger entries: SOL balance changes, SPL token transfers, and fees"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// Materializer for Solana token transfers.
pub struct SolanaTokenTransferMaterializer;

impl Materializer for SolanaTokenTransferMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::TokenTransfers
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:solana_token_transfers_v1_d4e9f1a7"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Solana
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Solana token transfers: SPL token movements extracted from pre/post token balances"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

/// Materializer for Solana native balance deltas.
pub struct SolanaNativeBalanceDeltaMaterializer;

impl Materializer for SolanaNativeBalanceDeltaMaterializer {
    fn dataset_name(&self) -> DatasetName {
        DatasetName::NativeBalanceDeltas
    }

    fn parser_version(&self) -> i32 {
        1
    }

    fn parser_hash(&self) -> &str {
        "sha256:solana_native_deltas_v1_b2c3a8f6"
    }

    fn chain_family(&self) -> ChainFamily {
        ChainFamily::Solana
    }

    fn descriptor(&self) -> DatasetDescriptor {
        DatasetDescriptor {
            name: self.dataset_name(),
            description:
                "Solana native balance deltas: SOL balance changes per account with fee payer flag"
                    .to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![self.chain_family()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_lamports_to_sol_large_positive() {
        let result = lamports_to_sol(10_000_000_000i128);
        assert_eq!(result, BigDecimal::from(10));
    }

    #[test]
    fn test_lamports_to_sol_negative() {
        let result = lamports_to_sol(-1_000_000_000i128);
        assert_eq!(result, BigDecimal::from(-1));
    }

    #[test]
    fn test_lamports_to_sol_beyond_i64_max() {
        let large: i128 = i64::MAX as i128 + 1_000_000_000;
        let result = lamports_to_sol(large);
        let expected = BigDecimal::from_str(&format!("{}", large)).unwrap()
            / BigDecimal::from_str("1000000000").unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn test_extract_sol_change_no_truncation() {
        use solana_transaction_status::UiTransactionStatusMeta;

        let pre_val: u64 = u64::MAX - 1_000_000_000;
        let post_val: u64 = u64::MAX;
        let meta = UiTransactionStatusMeta {
            err: None,
            status: Ok(()),
            fee: 0,
            pre_balances: vec![pre_val],
            post_balances: vec![post_val],
            inner_instructions:
                solana_transaction_status::option_serializer::OptionSerializer::None,
            log_messages: solana_transaction_status::option_serializer::OptionSerializer::None,
            pre_token_balances:
                solana_transaction_status::option_serializer::OptionSerializer::None,
            post_token_balances:
                solana_transaction_status::option_serializer::OptionSerializer::None,
            rewards: solana_transaction_status::option_serializer::OptionSerializer::None,
            loaded_addresses: solana_transaction_status::option_serializer::OptionSerializer::None,
            return_data: solana_transaction_status::option_serializer::OptionSerializer::None,
            compute_units_consumed:
                solana_transaction_status::option_serializer::OptionSerializer::None,
            cost_units: solana_transaction_status::option_serializer::OptionSerializer::None,
        };

        let change = extract_sol_change_lamports(&meta, 0);
        assert_eq!(change, 1_000_000_000i128);
    }

    #[test]
    fn solana_materializer_contract() {
        let m = SolanaLedgerMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::LedgerEntries);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Solana);
        assert!(!m.parser_hash().is_empty());
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::LedgerEntries);
        assert_eq!(desc.chain_families, vec![ChainFamily::Solana]);
    }

    #[test]
    fn solana_token_transfer_materializer_contract() {
        let m = SolanaTokenTransferMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::TokenTransfers);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Solana);
        assert!(!m.parser_hash().is_empty());
        assert_ne!(
            m.parser_hash(),
            SolanaLedgerMaterializer.parser_hash(),
            "distinct from ledger materializer"
        );
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::TokenTransfers);
    }

    #[test]
    fn solana_native_balance_delta_materializer_contract() {
        let m = SolanaNativeBalanceDeltaMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::NativeBalanceDeltas);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.chain_family(), ChainFamily::Solana);
        assert!(!m.parser_hash().is_empty());
        assert_ne!(
            m.parser_hash(),
            SolanaLedgerMaterializer.parser_hash(),
            "distinct from ledger materializer"
        );
        assert_ne!(
            m.parser_hash(),
            SolanaTokenTransferMaterializer.parser_hash(),
            "distinct from token transfer materializer"
        );
        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, DatasetName::NativeBalanceDeltas);
    }

    #[test]
    fn solana_all_materializers_have_distinct_hashes() {
        use std::collections::HashSet;

        let materializers: Vec<Box<dyn Materializer>> = vec![
            Box::new(SolanaLedgerMaterializer),
            Box::new(SolanaTokenTransferMaterializer),
            Box::new(SolanaNativeBalanceDeltaMaterializer),
        ];
        let hashes: HashSet<&str> = materializers.iter().map(|m| m.parser_hash()).collect();
        assert_eq!(
            hashes.len(),
            3,
            "all 3 Solana materializers must have distinct hashes"
        );
    }
}
