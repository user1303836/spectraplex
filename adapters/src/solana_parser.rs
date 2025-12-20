use spectraplex_core::models::{Transaction, LedgerEntry, EntryType};
use solana_transaction_status::{EncodedConfirmedTransactionWithStatusMeta, UiTransactionStatusMeta};
use solana_transaction_status::option_serializer::OptionSerializer;
use uuid::Uuid;
use bigdecimal::{BigDecimal, FromPrimitive};

pub fn parse_solana_transaction(tx: &Transaction) -> anyhow::Result<Vec<LedgerEntry>> {
    let mut entries = Vec::new();
    
    // 1. Deserialize the raw metadata back to the Solana SDK structure
    // Note: We are using the structure from `solana_transaction_status` which matches what `get_transaction` returns (EncodedConfirmedTransactionWithStatusMeta)
    let sol_tx: EncodedConfirmedTransactionWithStatusMeta = serde_json::from_value(tx.raw_metadata.clone())?;
    
    // Safety check: Ensure meta exists
    let meta = match &sol_tx.transaction.meta {
        Some(m) => m,
        None => return Ok(vec![]),
    };

    // 2. Extract Native SOL Changes
    // We need to look at account_keys to find the index of `tx.wallet_address`
    let transaction = &sol_tx.transaction.transaction;
    if let solana_transaction_status::EncodedTransaction::Json(ui_tx) = transaction {
            if let solana_transaction_status::UiMessage::Parsed(message) = &ui_tx.message {
                // Find index of wallet in account_keys
                if let Some(idx) = message.account_keys.iter().position(|k| k.pubkey == tx.wallet_address) {
                    let sol_change = extract_sol_change(meta, idx);
                    
                    if sol_change.abs() > 0.000001 {
                        entries.push(LedgerEntry {
                            id: Uuid::new_v4(),
                            transaction_id: tx.id,
                            user_id: tx.user_id,
                            wallet_address: tx.wallet_address.clone(),
                            asset_symbol: "SOL".to_string(),
                            amount: BigDecimal::from_f64(sol_change).unwrap_or_default(),
                            entry_type: if sol_change > 0.0 { EntryType::Transfer } else { EntryType::Transfer }, // Simplified for now
                            fiat_value: None,
                        });
                    }
                }
            }
    }

    // 3. Extract SPL Token Changes
    if let OptionSerializer::Some(pre_token_balances) = &meta.pre_token_balances {
        if let OptionSerializer::Some(post_token_balances) = &meta.post_token_balances {
            
            // We iterate over post_token_balances where owner == wallet_address
            for post in post_token_balances {
                let owner_match = match &post.owner {
                    OptionSerializer::Some(owner) => owner == &tx.wallet_address,
                    OptionSerializer::None => false,
                    OptionSerializer::Skip => false,
                };

                if owner_match {
                    let mint = post.mint.clone();
                    
                    // Find corresponding pre-balance for this account index
                    let pre_amount = pre_token_balances.iter()
                       .find(|p| p.account_index == post.account_index)
                       .map(|p| p.ui_token_amount.ui_amount.unwrap_or(0.0))
                       .unwrap_or(0.0); // If not found, it's a new token account (0 balance)

                    let post_amount = post.ui_token_amount.ui_amount.unwrap_or(0.0);
                    let delta = post_amount - pre_amount;

                    if delta.abs() > 0.000001 {
                         entries.push(LedgerEntry {
                             id: Uuid::new_v4(),
                             transaction_id: tx.id,
                             user_id: tx.user_id,
                             wallet_address: tx.wallet_address.clone(),
                             asset_symbol: mint,
                             amount: BigDecimal::from_f64(delta).unwrap_or_default(),
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

fn extract_sol_change(meta: &UiTransactionStatusMeta, wallet_index: usize) -> f64 {
    let pre = meta.pre_balances.get(wallet_index).copied().unwrap_or(0) as f64;
    let post = meta.post_balances.get(wallet_index).copied().unwrap_or(0) as f64;
    (post - pre) / 1_000_000_000.0 // Lamports to SOL
}
