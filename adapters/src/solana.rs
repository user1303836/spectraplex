use spectraplex_core::models::{Chain, Transaction, ChainIngestor};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::UiTransactionEncoding;
use std::str::FromStr;
use uuid::Uuid;
use serde_json::json;

pub struct SolanaAdapter {
    client: RpcClient,
}

impl SolanaAdapter {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            client: RpcClient::new(rpc_url.to_string()),
        }
    }
}

#[async_trait::async_trait]
impl ChainIngestor for SolanaAdapter {
    async fn fetch_history(&self, wallet: &str, limit: usize) -> anyhow::Result<Vec<Transaction>> {
        let pubkey = Pubkey::from_str(wallet)?;
        // Fetch signatures (transaction history list)
        let signatures = self.client.get_signatures_for_address(&pubkey)?;
        
        let mut transactions = Vec::new();

        for sig_info in signatures.iter().take(limit) {
            let sig = Signature::from_str(&sig_info.signature)?;
            
            // Fetch full transaction details in JSON format
            // We use UiTransactionEncoding::JsonParsed to get as much detail as possible, 
            // or Json for raw structure. "Json" is often safer for raw storage.
            match self.client.get_transaction(&sig, UiTransactionEncoding::Json) {
                Ok(tx) => {
                    // Serialize the entire response to a JSON Value
                    let raw_metadata = serde_json::to_value(&tx).unwrap_or(json!({}));

                    transactions.push(Transaction {
                        id: Uuid::new_v4(),
                        user_id: Uuid::nil(), // Placeholder
                        wallet_address: wallet.to_string(),
                        timestamp: tx.block_time.unwrap_or(0),
                        tx_hash: sig_info.signature.clone(),
                        chain: Chain::Solana,
                        raw_metadata,
                    });
                }
                Err(e) => {
                    eprintln!("Failed to fetch tx {}: {}", sig_info.signature, e);
                }
            }
        }

        Ok(transactions)
    }
}
