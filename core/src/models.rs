use serde::{Deserialize, Serialize};
use uuid::Uuid;
use bigdecimal::BigDecimal;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Chain {
    Solana,
    Hyperliquid,
    Ethereum,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EntryType {
    Trade,
    Fee,
    Transfer,
    Staking,
    Income,
}

// Bronze Layer: Raw Immutable Data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: Uuid,
    pub user_id: Uuid,
    pub wallet_address: String, // The wallet we are tracking
    pub timestamp: i64,
    pub tx_hash: String,
    pub chain: Chain,
    pub raw_metadata: serde_json::Value,
}

// Silver Layer: Normalized Financial Data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: Uuid,
    pub transaction_id: Uuid,
    pub user_id: Uuid,
    pub wallet_address: String,
    pub asset_symbol: String,
    pub amount: BigDecimal, 
    pub entry_type: EntryType,
    pub fiat_value: Option<BigDecimal>,
}

#[async_trait::async_trait]
pub trait ChainIngestor {
    async fn fetch_history(&self, wallet: &str, limit: usize) -> anyhow::Result<Vec<Transaction>>;
}