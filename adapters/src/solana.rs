// Notes: Use HTTP to backfill data
// Switch to grpc to listen for new transactions to prevent refetching via http

use core::models::{Chain, EventType, TaxEvent};
use core::models::ChainIngestor;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{UiTransactionEncoding, OptionSerializer};
use std::str::FromStr;

pub struct SolanaAdapter {
    client: RpcClient,
}

impl SolanaAdapter {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            client: RpcClient::new(rpc_url.to_string()),
        }
    }

    // Helper to calculate SOL (Native) changes
    fn extract_sol_change(&self, meta: &solana_transaction_status::UiTransactionStatusMeta, wallet_index: usize) -> f64 {
        let pre = meta.pre_balances[wallet_index] as f64;
        let post = meta.post_balances[wallet_index] as f64;
        (post - pre) / 1_000_000_000.0 // Lamports to SOL
    }
}

#[async_trait::async_trait]
impl ChainIngestor for SolanaAdapter {
    async fn fetch_history(&self, wallet: &str, limit: usize) -> anyhow::Result<Vec<TaxEvent>> {
        let pubkey = Pubkey::from_str(wallet)?;
        
        // 1. Fetch Signatures (Tx IDs)
        // In production, you would handle pagination here (using `before` parameter)
        let signatures = self.client.get_signatures_for_address(&pubkey)?;
        
        let mut tax_events = Vec::new();

        // 2. Process transactions (Batching recommended for Prod, doing linear for prototype)
        for sig_info in signatures.iter().take(limit) {
            let sig = Signature::from_str(&sig_info.signature)?;
            
            // Fetch full transaction details
            let tx = self.client.get_transaction(&sig, UiTransactionEncoding::Json)?;
            let meta = tx.transaction.meta.ok_or(anyhow::anyhow!("No meta found"))?;
            let block_time = tx.block_time.unwrap_or(0);

            // --- PARSING STRATEGY: Balance Diffs ---
            
            // A. Handle Token Changes (SPL Tokens)
            if let OptionSerializer::Some(pre_token_balances) = &meta.pre_token_balances {
                if let OptionSerializer::Some(post_token_balances) = &meta.post_token_balances {
                    
                    // Find all tokens this wallet touched
                    // Note: This logic assumes we match mints. 
                    // Real-world: You need to map `account_index` to handle multiple accounts for same mint.
                    
                    for post in post_token_balances {
                        if post.owner.as_deref() == Some(wallet) {
                            let mint = post.mint.clone();
                            
                            // Find corresponding pre-balance
                            let pre_amount = pre_token_balances.iter()
                               .find(|p| p.account_index == post.account_index)
                               .map(|p| p.ui_token_amount.ui_amount.unwrap_or(0.0))
                               .unwrap_or(0.0); // If no pre, it was 0 (received new token)

                            let post_amount = post.ui_token_amount.ui_amount.unwrap_or(0.0);
                            let delta = post_amount - pre_amount;

                            if delta.abs() > 0.000001 {
                                tax_events.push(TaxEvent {
                                    chain: Chain::Solana,
                                    tx_hash: sig_info.signature.clone(),
                                    block_time,
                                    event_type: if delta > 0.0 { EventType::TransferIn } else { EventType::TransferOut },
                                    asset_symbol: "UNKNOWN_SPL".to_string(), // TODO: Fetch Mint Metadata
                                    asset_address: mint,
                                    amount_change: delta,
                                    fee_paid: (meta.fee as f64) / 1_000_000_000.0, // Fee is technically paid by payer, verify if wallet == payer
                                    fee_asset: "SOL".to_string(),
                                });
                            }
                        }
                    }
                }
            }
            
            // B. Handle SOL Changes (Native)
            // We need to find the index of our wallet in the account keys list to check pre/post balances
            if let Some(transaction) = tx.transaction.transaction {
                if let solana_transaction_status::EncodedTransaction::Json(ui_tx) = transaction {
                     if let solana_transaction_status::UiMessage::Parsed(message) = ui_tx.message {
                         // Find index of wallet in account_keys
                         if let Some(idx) = message.account_keys.iter().position(|k| k.pubkey == wallet) {
                             let sol_change = self.extract_sol_change(&meta, idx);
                             
                             // Only log if significant change (filter out rent noise if needed)
                             if sol_change.abs() > 0.000001 {
                                 tax_events.push(TaxEvent {
                                     chain: Chain::Solana,
                                     tx_hash: sig_info.signature.clone(),
                                     block_time,
                                     event_type: if sol_change > 0.0 { EventType::TransferIn } else { EventType::TransferOut },
                                     asset_symbol: "SOL".to_string(),
                                     asset_address: "Native".to_string(),
                                     amount_change: sol_change,
                                     fee_paid: 0.0, // Already accounted for in the balance change if payer
                                     fee_asset: "SOL".to_string(),
                                 });
                             }
                         }
                     }
                }
            }
        }

        Ok(tax_events)
    }
}