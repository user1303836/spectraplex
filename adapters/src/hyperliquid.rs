use spectraplex_core::models::{Chain, ChainIngestor, Transaction};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const HL_INFO_URL: &str = "https://api.hyperliquid.xyz/info";

pub struct HyperliquidAdapter {
    client: reqwest::Client,
    base_url: String,
}

// --- Request types ---

#[derive(Serialize)]
struct UserFillsRequest<'a> {
    r#type: &'static str,
    user: &'a str,
}

#[derive(Serialize)]
struct UserFundingRequest<'a> {
    r#type: &'static str,
    user: &'a str,
    #[serde(rename = "startTime")]
    start_time: i64,
}

#[derive(Serialize)]
struct UserLedgerUpdatesRequest<'a> {
    r#type: &'static str,
    user: &'a str,
    #[serde(rename = "startTime")]
    start_time: i64,
}

// --- Response types (for deserialization) ---

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HlFill {
    pub coin: String,
    pub px: String,
    pub sz: String,
    pub side: String,
    pub time: u64,
    pub hash: String,
    #[serde(rename = "startPosition")]
    pub start_position: Option<String>,
    pub dir: Option<String>,
    #[serde(rename = "closedPnl")]
    pub closed_pnl: Option<String>,
    pub fee: Option<String>,
    pub tid: Option<u64>,
    pub oid: Option<u64>,
    #[serde(rename = "feeToken")]
    pub fee_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HlFundingEntry {
    pub time: u64,
    pub coin: String,
    #[serde(rename = "usdc")]
    pub usdc: String,
    #[serde(rename = "fundingRate")]
    pub funding_rate: Option<String>,
    pub hash: Option<String>,
    #[serde(rename = "nSamples")]
    pub n_samples: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HlLedgerUpdate {
    pub time: u64,
    pub hash: String,
    pub delta: serde_json::Value,
}

impl Default for HyperliquidAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperliquidAdapter {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: HL_INFO_URL.to_string(),
        }
    }

    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.to_string(),
        }
    }

    pub async fn fetch_user_fills(&self, wallet: &str) -> anyhow::Result<Vec<HlFill>> {
        let body = UserFillsRequest {
            r#type: "userFills",
            user: wallet,
        };
        let resp = self
            .client
            .post(&self.base_url)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Hyperliquid userFills returned {}: {}", status, text);
        }
        let fills: Vec<HlFill> = resp.json().await?;
        Ok(fills)
    }

    pub async fn fetch_user_funding(
        &self,
        wallet: &str,
        start_time: i64,
    ) -> anyhow::Result<Vec<HlFundingEntry>> {
        let body = UserFundingRequest {
            r#type: "userFunding",
            user: wallet,
            start_time,
        };
        let resp = self
            .client
            .post(&self.base_url)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Hyperliquid userFunding returned {}: {}", status, text);
        }
        let funding: Vec<HlFundingEntry> = resp.json().await?;
        Ok(funding)
    }

    pub async fn fetch_user_ledger_updates(
        &self,
        wallet: &str,
        start_time: i64,
    ) -> anyhow::Result<Vec<HlLedgerUpdate>> {
        let body = UserLedgerUpdatesRequest {
            r#type: "userNonFundingLedgerUpdates",
            user: wallet,
            start_time,
        };
        let resp = self
            .client
            .post(&self.base_url)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Hyperliquid userNonFundingLedgerUpdates returned {}: {}",
                status,
                text
            );
        }
        let updates: Vec<HlLedgerUpdate> = resp.json().await?;
        Ok(updates)
    }
}

#[async_trait::async_trait]
impl ChainIngestor for HyperliquidAdapter {
    async fn fetch_history(
        &self,
        wallet: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Transaction>> {
        let mut transactions = Vec::new();

        // 1. Fetch fills (trades)
        let fills = self.fetch_user_fills(wallet).await?;
        for fill in fills.iter().take(limit) {
            let raw = serde_json::to_value(fill)?;
            transactions.push(Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::nil(),
                wallet_address: wallet.to_string(),
                timestamp: (fill.time / 1000) as i64, // HL timestamps are in ms
                tx_hash: fill.hash.clone(),
                chain: Chain::Hyperliquid,
                raw_metadata: serde_json::json!({ "type": "fill", "data": raw }),
            });
        }

        // 2. Fetch funding payments (from epoch 0 to get all)
        let funding = self.fetch_user_funding(wallet, 0).await?;
        for entry in funding.iter().take(limit) {
            let raw = serde_json::to_value(entry)?;
            let hash = entry
                .hash
                .clone()
                .unwrap_or_else(|| format!("funding-{}-{}", entry.coin, entry.time));
            transactions.push(Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::nil(),
                wallet_address: wallet.to_string(),
                timestamp: (entry.time / 1000) as i64,
                tx_hash: hash,
                chain: Chain::Hyperliquid,
                raw_metadata: serde_json::json!({ "type": "funding", "data": raw }),
            });
        }

        // 3. Fetch non-funding ledger updates (deposits, withdrawals, liquidations)
        let ledger_updates = self.fetch_user_ledger_updates(wallet, 0).await?;
        for update in ledger_updates.iter().take(limit) {
            let raw = serde_json::to_value(update)?;
            transactions.push(Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::nil(),
                wallet_address: wallet.to_string(),
                timestamp: (update.time / 1000) as i64,
                tx_hash: update.hash.clone(),
                chain: Chain::Hyperliquid,
                raw_metadata: serde_json::json!({ "type": "ledger_update", "data": raw }),
            });
        }

        Ok(transactions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fills_json() -> &'static str {
        r#"[
            {
                "coin": "ETH",
                "px": "3500.0",
                "sz": "1.5",
                "side": "B",
                "time": 1700000000000,
                "hash": "0xabc123",
                "startPosition": "0.0",
                "dir": "Open Long",
                "closedPnl": "0.0",
                "fee": "1.75",
                "tid": 12345,
                "oid": 67890,
                "feeToken": "USDC"
            }
        ]"#
    }

    fn sample_funding_json() -> &'static str {
        r#"[
            {
                "time": 1700000000000,
                "coin": "ETH",
                "usdc": "-2.50",
                "fundingRate": "0.0001",
                "hash": "0xfund123",
                "nSamples": 1
            }
        ]"#
    }

    fn sample_ledger_updates_json() -> &'static str {
        r#"[
            {
                "time": 1700000000000,
                "hash": "0xledger123",
                "delta": {
                    "type": "deposit",
                    "usdc": "10000.0"
                }
            }
        ]"#
    }

    #[test]
    fn test_deserialize_fills() {
        let fills: Vec<HlFill> = serde_json::from_str(sample_fills_json()).unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].coin, "ETH");
        assert_eq!(fills[0].side, "B");
        assert_eq!(fills[0].px, "3500.0");
        assert_eq!(fills[0].sz, "1.5");
        assert_eq!(fills[0].fee.as_deref(), Some("1.75"));
    }

    #[test]
    fn test_deserialize_funding() {
        let funding: Vec<HlFundingEntry> =
            serde_json::from_str(sample_funding_json()).unwrap();
        assert_eq!(funding.len(), 1);
        assert_eq!(funding[0].coin, "ETH");
        assert_eq!(funding[0].usdc, "-2.50");
        assert_eq!(funding[0].funding_rate.as_deref(), Some("0.0001"));
    }

    #[test]
    fn test_deserialize_ledger_updates() {
        let updates: Vec<HlLedgerUpdate> =
            serde_json::from_str(sample_ledger_updates_json()).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].hash, "0xledger123");
        assert_eq!(updates[0].delta["type"], "deposit");
    }

    #[tokio::test]
    async fn test_fetch_history_with_mock_server() {
        // Start a mock HTTP server
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);

        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut stream = stream;
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]);

                    // Parse the JSON body to determine request type
                    let response_body = if request.contains("userFills") {
                        r#"[{"coin":"BTC","px":"40000.0","sz":"0.1","side":"A","time":1700000000000,"hash":"0xtest1"}]"#
                    } else if request.contains("userFunding") {
                        r#"[{"time":1700000000000,"coin":"BTC","usdc":"1.23"}]"#
                    } else if request.contains("userNonFundingLedgerUpdates") {
                        r#"[{"time":1700000000000,"hash":"0xledger1","delta":{"type":"deposit","usdc":"5000.0"}}]"#
                    } else {
                        "[]"
                    };

                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });

        let adapter = HyperliquidAdapter::with_base_url(&base_url);
        let txs = adapter
            .fetch_history("0x1234567890abcdef1234567890abcdef12345678", 10)
            .await
            .unwrap();

        // Should have 3 transactions: 1 fill + 1 funding + 1 ledger update
        assert_eq!(txs.len(), 3);

        // Check fill transaction
        let fill_tx = &txs[0];
        assert_eq!(fill_tx.raw_metadata["type"], "fill");
        assert_eq!(fill_tx.raw_metadata["data"]["coin"], "BTC");
        assert_eq!(fill_tx.tx_hash, "0xtest1");

        // Check funding transaction
        let funding_tx = &txs[1];
        assert_eq!(funding_tx.raw_metadata["type"], "funding");

        // Check ledger update transaction
        let ledger_tx = &txs[2];
        assert_eq!(ledger_tx.raw_metadata["type"], "ledger_update");

        server.abort();
    }
}
