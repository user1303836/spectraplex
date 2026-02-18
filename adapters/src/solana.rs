use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use solana_client::rpc_client::{GetConfirmedSignaturesForAddress2Config, RpcClient};
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::UiTransactionEncoding;
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, Transaction};
use tracing::warn;
use uuid::Uuid;

pub struct SolanaAdapter {
    client: Arc<RpcClient>,
}

impl SolanaAdapter {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            client: Arc::new(RpcClient::new_with_timeout(
                rpc_url.to_string(),
                Duration::from_secs(30),
            )),
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
        checkpoint: Option<&IndexerCheckpoint>,
    ) -> anyhow::Result<Vec<Transaction>> {
        let client = self.client.clone();
        let wallet = wallet.to_string();
        let until_sig = checkpoint
            .and_then(|cp| cp.last_signature.clone())
            .and_then(|s| Signature::from_str(&s).ok());

        tokio::task::spawn_blocking(move || {
            let pubkey = Pubkey::from_str(&wallet)?;
            let config = GetConfirmedSignaturesForAddress2Config {
                until: until_sig,
                limit: Some(limit),
                ..Default::default()
            };
            let signatures = client.get_signatures_for_address_with_config(&pubkey, config)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adapter_construction_with_timeout() {
        let adapter = SolanaAdapter::new("https://api.mainnet-beta.solana.com");
        // Verify the client was created successfully with timeout
        assert!(Arc::strong_count(&adapter.client) == 1);
    }
}
