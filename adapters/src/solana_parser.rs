use bigdecimal::BigDecimal;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, UiTransactionStatusMeta,
};
use spectraplex_core::models::{EntryType, LedgerEntry, Transaction};
use std::str::FromStr;
use uuid::Uuid;

pub fn parse_solana_transaction(tx: &Transaction) -> anyhow::Result<Vec<LedgerEntry>> {
    let mut entries = Vec::new();

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
                    let fee_amount = lamports_to_sol(-(fee_lamports as i64));
                    entries.push(LedgerEntry {
                        id: Uuid::new_v4(),
                        transaction_id: tx.id,
                        user_id: tx.user_id,
                        wallet_address: tx.wallet_address.clone(),
                        asset_symbol: "SOL".to_string(),
                        amount: fee_amount,
                        entry_type: EntryType::Fee,
                        fiat_value: None,
                    });

                    // Net transfer amount = total balance change + fee (since fee is included in the balance change)
                    let transfer_lamports = lamport_change + fee_lamports as i64;
                    if transfer_lamports != 0 {
                        let transfer_amount = lamports_to_sol(transfer_lamports);
                        entries.push(LedgerEntry {
                            id: Uuid::new_v4(),
                            transaction_id: tx.id,
                            user_id: tx.user_id,
                            wallet_address: tx.wallet_address.clone(),
                            asset_symbol: "SOL".to_string(),
                            amount: transfer_amount,
                            entry_type: EntryType::Transfer,
                            fiat_value: None,
                        });
                    }
                } else if lamport_change != 0 {
                    // Non-fee-payer: entire balance change is a transfer
                    let amount = lamports_to_sol(lamport_change);
                    entries.push(LedgerEntry {
                        id: Uuid::new_v4(),
                        transaction_id: tx.id,
                        user_id: tx.user_id,
                        wallet_address: tx.wallet_address.clone(),
                        asset_symbol: "SOL".to_string(),
                        amount,
                        entry_type: EntryType::Transfer,
                        fiat_value: None,
                    });
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
                        entries.push(LedgerEntry {
                            id: Uuid::new_v4(),
                            transaction_id: tx.id,
                            user_id: tx.user_id,
                            wallet_address: tx.wallet_address.clone(),
                            asset_symbol: mint,
                            amount,
                            entry_type: EntryType::Transfer,
                            fiat_value: None,
                        });
                    }
                }
            }
        }
    }

    Ok(entries)
}

/// Extract the raw lamport balance change for a given account index.
fn extract_sol_change_lamports(meta: &UiTransactionStatusMeta, wallet_index: usize) -> i64 {
    let pre = meta.pre_balances.get(wallet_index).copied().unwrap_or(0) as i64;
    let post = meta.post_balances.get(wallet_index).copied().unwrap_or(0) as i64;
    post - pre
}

/// Convert lamports (i64) to SOL as BigDecimal without floating-point precision loss.
fn lamports_to_sol(lamports: i64) -> BigDecimal {
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
