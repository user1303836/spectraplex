use std::num::NonZeroU32;
use std::sync::Arc;

use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, Log};
use governor::{Quota, RateLimiter};
use serde_json::json;
use spectraplex_core::models::{Chain, ChainIngestor, Transaction};
use uuid::Uuid;

/// Maximum block range per eth_getLogs request.
const DEFAULT_BLOCK_CHUNK: u64 = 2000;

/// EVM chain adapter that fetches logs and transactions via JSON-RPC.
pub struct EvmAdapter {
    provider: Arc<dyn Provider + Send + Sync>,
    rate_limiter: Arc<
        RateLimiter<
            governor::state::NotKeyed,
            governor::state::InMemoryState,
            governor::clock::DefaultClock,
        >,
    >,
    block_chunk: u64,
}

impl EvmAdapter {
    /// Create a new adapter connected to an EVM-compatible RPC endpoint.
    pub async fn new(rpc_url: &str) -> anyhow::Result<Self> {
        let provider = ProviderBuilder::new().connect(rpc_url).await?;

        // 5 requests per second by default (safe for public RPCs)
        let quota = Quota::per_second(NonZeroU32::new(5).unwrap());
        let limiter = RateLimiter::direct(quota);

        Ok(Self {
            provider: Arc::new(provider),
            rate_limiter: Arc::new(limiter),
            block_chunk: DEFAULT_BLOCK_CHUNK,
        })
    }

    /// Set a custom block chunk size for `eth_getLogs` range queries.
    pub fn with_block_chunk(mut self, chunk: u64) -> Self {
        self.block_chunk = chunk;
        self
    }

    /// Fetch logs for a given address across a block range, chunked to avoid
    /// RPC limits.
    pub async fn fetch_logs(
        &self,
        address: Address,
        from_block: u64,
        to_block: u64,
    ) -> anyhow::Result<Vec<Log>> {
        let mut all_logs = Vec::new();
        let mut start = from_block;

        while start <= to_block {
            let end = std::cmp::min(start + self.block_chunk - 1, to_block);

            self.rate_limiter.until_ready().await;

            let filter = Filter::new()
                .address(address)
                .from_block(start)
                .to_block(end);

            let logs = self.provider.get_logs(&filter).await?;
            all_logs.extend(logs);

            start = end + 1;
        }

        Ok(all_logs)
    }

    /// Check block parent hash continuity for reorg detection.
    /// Returns `Some(fork_block)` if a reorg is detected, starting from
    /// the block where parent_hash diverges.
    pub async fn detect_reorg(
        &self,
        known_blocks: &[(u64, B256)], // (block_num, block_hash) sorted ascending
    ) -> anyhow::Result<Option<u64>> {
        for (block_num, known_hash) in known_blocks.iter().rev() {
            self.rate_limiter.until_ready().await;

            if let Some(block) = self
                .provider
                .get_block_by_number((*block_num).into())
                .await?
            {
                if block.header.hash != *known_hash {
                    return Ok(Some(*block_num));
                }
            }
        }
        Ok(None)
    }
}

#[async_trait::async_trait]
impl ChainIngestor for EvmAdapter {
    async fn fetch_history(
        &self,
        wallet: &str,
        limit: usize,
        user_id: Uuid,
    ) -> anyhow::Result<Vec<Transaction>> {
        let address: Address = wallet.parse()?;

        self.rate_limiter.until_ready().await;
        let latest_block = self.provider.get_block_number().await?;

        // Scan backwards from head — limit translates to how many chunks to scan.
        let total_blocks = (limit as u64) * self.block_chunk;
        let from_block = latest_block.saturating_sub(total_blocks);

        let logs = self.fetch_logs(address, from_block, latest_block).await?;

        let mut transactions = Vec::new();

        for log in &logs {
            let tx_hash = match log.transaction_hash {
                Some(h) => format!("{h:#x}"),
                None => continue,
            };

            let block_timestamp = log.block_timestamp.unwrap_or(0);

            let raw_metadata = json!({
                "log_index": log.log_index,
                "block_number": log.block_number,
                "block_hash": log.block_hash.map(|h| format!("{h:#x}")),
                "transaction_hash": &tx_hash,
                "address": format!("{:#x}", log.address()),
                "topics": log.topics().iter().map(|t| format!("{t:#x}")).collect::<Vec<_>>(),
                "data": format!("0x{}", alloy::hex::encode(log.data().data.as_ref())),
            });

            transactions.push(Transaction {
                id: Uuid::new_v4(),
                user_id,
                wallet_address: wallet.to_string(),
                timestamp: block_timestamp as i64,
                tx_hash,
                chain: Chain::Ethereum,
                raw_metadata,
            });
        }

        Ok(transactions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adapter_block_chunk_default() {
        // Verify default constant is sane
        assert_eq!(DEFAULT_BLOCK_CHUNK, 2000);
    }
}
