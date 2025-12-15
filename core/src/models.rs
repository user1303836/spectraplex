use serde::{Deserialize, Serialize};

pub enum Chain {
    Solana,
    Hyperliquid,
    Ethereum,
}

pub enum EventType {
    Trade,
    TransferIn,
    TransferOut,
    Income,
    Fee,
}

pub struct TaxEvent {
    pub chain: Chain,
    pub tx_hash: String,
    pub block_time: i64,
    pub event_type: EventType,

    pub asset_symbol: String,
    pub asset_address: String, // Mint address or Contract address
    pub amount_change: f64, // Positive = Received, Negative = Sent
    
    // Fees are always relevant for tax cost-basis
    pub fee_paid: f64, 
    pub fee_asset: String,
}

#[async_trait::async_trait]
pub trait ChainIngestor {
    async fn fetch_history(&self, wallet: &str, limit: usize) -> anyhow::Result<Vec<TaxEvent>>;
}