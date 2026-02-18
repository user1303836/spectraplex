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
}
