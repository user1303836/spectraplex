use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::UiTransactionEncoding;
use spectraplex_core::models::{Chain, ChainIngestor, Transaction};
use std::str::FromStr;
use std::sync::Arc;
use tracing::warn;
use uuid::Uuid;

pub struct SolanaAdapter {
    client: Arc<RpcClient>,
}

impl SolanaAdapter {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            client: Arc::new(RpcClient::new(rpc_url.to_string())),
        }
    }
}

#[async_trait::async_trait]
impl ChainIngestor for SolanaAdapter {
    async fn fetch_history(
        &self,
        wallet: &str,
        limit: usize,
        user_id: Uuid,
    ) -> anyhow::Result<Vec<Transaction>> {
        let client = self.client.clone();
        let wallet = wallet.to_string();

        tokio::task::spawn_blocking(move || {
            let pubkey = Pubkey::from_str(&wallet)?;
            let signatures = client.get_signatures_for_address(&pubkey)?;

            let mut transactions = Vec::new();

            for sig_info in signatures.iter().take(limit) {
                let sig = Signature::from_str(&sig_info.signature)?;

                match client.get_transaction(&sig, UiTransactionEncoding::Json) {
                    Ok(tx) => {
                        let raw_metadata = match serde_json::to_value(&tx) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!(
                                    tx_hash = %sig_info.signature,
                                    error = %e,
                                    "Failed to serialize transaction metadata, using empty object"
                                );
                                serde_json::Value::Object(Default::default())
                            }
                        };

                        transactions.push(Transaction {
                            id: Uuid::new_v4(),
                            user_id,
                            wallet_address: wallet.to_string(),
                            timestamp: tx.block_time.unwrap_or(0),
                            tx_hash: sig_info.signature.clone(),
                            chain: Chain::Solana,
                            raw_metadata,
                        });
                    }
                    Err(e) => {
                        warn!(
                            tx_hash = %sig_info.signature,
                            error = %e,
                            "Failed to fetch transaction, skipping"
                        );
                    }
                }
            }

            Ok(transactions)
        })
        .await?
    }
}
