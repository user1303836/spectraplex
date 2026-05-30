use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use spectraplex_core::connector::{ConnectorCapabilities, MarketFilterSpec};
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, Transaction};
use spectraplex_core::provider::{NetworkContext, ProviderCapability};
use spectraplex_core::v2::{
    ChainFamily, IndexTarget, IngestionBatch, RawTransaction, TargetKind, TargetMode,
};
use tokio::sync::mpsc;
use tracing::info;
use uuid::Uuid;

use crate::hyperliquid_ws::HyperliquidWsClient;

const HL_INFO_URL: &str = "https://api.hyperliquid.xyz/info";

async fn ensure_success_response(
    resp: reqwest::Response,
    endpoint: &str,
) -> anyhow::Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }

    let text = match resp.text().await {
        Ok(text) if text.is_empty() => "<empty response body>".to_string(),
        Ok(text) => text,
        Err(e) => format!("<failed to read response body: {e}>"),
    };

    anyhow::bail!("Hyperliquid {endpoint} returned {status}: {text}");
}

pub struct HyperliquidAdapter {
    client: reqwest::Client,
    base_url: String,
    ws_url: Option<String>,
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

#[derive(Serialize)]
struct FundingHistoryRequest<'a> {
    r#type: &'static str,
    coin: &'a str,
    #[serde(rename = "startTime")]
    start_time: i64,
    #[serde(rename = "endTime", skip_serializing_if = "Option::is_none")]
    end_time: Option<i64>,
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

/// Market-level funding rate record from the `fundingHistory` REST endpoint.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HlFundingRate {
    pub coin: String,
    #[serde(rename = "fundingRate")]
    pub funding_rate: String,
    pub premium: String,
    pub time: u64,
}

impl Default for HyperliquidAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HyperliquidAdapter {
    pub fn new() -> Self {
        Self {
            client: Self::build_client(),
            base_url: HL_INFO_URL.to_string(),
            ws_url: None,
        }
    }

    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            client: Self::build_client(),
            base_url: base_url.to_string(),
            ws_url: None,
        }
    }

    pub fn with_urls(base_url: &str, ws_url: &str) -> Self {
        Self {
            client: Self::build_client(),
            base_url: base_url.to_string(),
            ws_url: Some(ws_url.to_string()),
        }
    }

    /// Create a new adapter from a `NetworkContext`.
    ///
    /// Resolves the REST URL from the `Historical` capability for the
    /// info API, and optionally the WS URL from the `Stream` capability.
    /// Falls back to the hardcoded defaults if no providers are available,
    /// preserving backward compatibility with the zero-arg constructor.
    pub fn from_network_context(ctx: &NetworkContext) -> Self {
        let base_url = ctx
            .url(ProviderCapability::Historical)
            .map(|u| {
                // The provider URL is the base (e.g. "https://api.hyperliquid.xyz").
                // The info endpoint appends /info. Normalize trailing slash.
                let base = u.trim_end_matches('/');
                if base.ends_with("/info") {
                    base.to_string()
                } else {
                    format!("{base}/info")
                }
            })
            .unwrap_or_else(|| HL_INFO_URL.to_string());

        let ws_url = ctx.url(ProviderCapability::Stream).map(|u| u.to_string());

        Self {
            client: Self::build_client(),
            base_url,
            ws_url,
        }
    }

    fn build_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client")
    }

    // -----------------------------------------------------------------------
    // User-level REST methods (wallet targets)
    // -----------------------------------------------------------------------

    pub async fn fetch_user_fills(&self, wallet: &str) -> anyhow::Result<Vec<HlFill>> {
        let body = UserFillsRequest {
            r#type: "userFills",
            user: wallet,
        };
        let resp = self.client.post(&self.base_url).json(&body).send().await?;
        let resp = ensure_success_response(resp, "userFills").await?;
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
        let resp = self.client.post(&self.base_url).json(&body).send().await?;
        let resp = ensure_success_response(resp, "userFunding").await?;
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
        let resp = self.client.post(&self.base_url).json(&body).send().await?;
        let resp = ensure_success_response(resp, "userNonFundingLedgerUpdates").await?;
        let updates: Vec<HlLedgerUpdate> = resp.json().await?;
        Ok(updates)
    }

    // -----------------------------------------------------------------------
    // Market-level REST methods (market targets)
    // -----------------------------------------------------------------------

    /// Fetch historical funding rates for a specific coin.
    ///
    /// Uses the `fundingHistory` REST endpoint which returns market-wide
    /// funding rate records (not per-user). This is the primary market-level
    /// backfill data source since Hyperliquid does not expose a `recentTrades`
    /// REST endpoint.
    pub async fn fetch_funding_history(
        &self,
        coin: &str,
        start_time: i64,
        end_time: Option<i64>,
    ) -> anyhow::Result<Vec<HlFundingRate>> {
        let body = FundingHistoryRequest {
            r#type: "fundingHistory",
            coin,
            start_time,
            end_time,
        };
        let resp = self.client.post(&self.base_url).json(&body).send().await?;
        let resp = ensure_success_response(resp, "fundingHistory").await?;
        let rates: Vec<HlFundingRate> = resp.json().await?;
        Ok(rates)
    }

    // -----------------------------------------------------------------------
    // V2 Connector internal backfill methods
    // -----------------------------------------------------------------------

    /// Wallet backfill: fetch fills, funding, and ledger updates for a wallet
    /// address, producing target-agnostic `RawTransaction` records.
    async fn wallet_backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        let wallet_str = target
            .address
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("wallet target must have an address"))?;

        let resume_ms = cursor_to_start_time_ms(cursor);
        let resume_ms_u64 = i64_to_u64_or_zero(resume_ms);
        let network = &target.network;
        let mut records = Vec::new();

        // 1. Fills — no startTime param, filter client-side
        let fills = self.fetch_user_fills(wallet_str).await?;
        for fill in fills.iter().filter(|f| f.time >= resume_ms_u64).take(limit) {
            let raw = serde_json::to_value(fill)?;
            records.push(RawTransaction {
                id: Uuid::new_v4(),
                network: network.clone(),
                tx_hash: fill.hash.clone(),
                timestamp: unix_ms_to_secs_i64(fill.time),
                block_number: None,
                raw_metadata: serde_json::json!({ "type": "fill", "data": raw }),
                source: "hl-rest-wallet-backfill".to_string(),
                ingestion_run_id: None,
                ingested_at: Utc::now(),
            });
        }

        // 2. Funding payments
        let remaining = limit.saturating_sub(records.len());
        if remaining > 0 {
            let funding = self.fetch_user_funding(wallet_str, resume_ms).await?;
            for entry in funding.iter().take(remaining) {
                let raw = serde_json::to_value(entry)?;
                let hash = entry
                    .hash
                    .clone()
                    .unwrap_or_else(|| format!("funding-{}-{}", entry.coin, entry.time));
                records.push(RawTransaction {
                    id: Uuid::new_v4(),
                    network: network.clone(),
                    tx_hash: hash,
                    timestamp: unix_ms_to_secs_i64(entry.time),
                    block_number: None,
                    raw_metadata: serde_json::json!({ "type": "funding", "data": raw }),
                    source: "hl-rest-wallet-backfill".to_string(),
                    ingestion_run_id: None,
                    ingested_at: Utc::now(),
                });
            }
        }

        // 3. Non-funding ledger updates
        let remaining = limit.saturating_sub(records.len());
        if remaining > 0 {
            let ledger_updates = self
                .fetch_user_ledger_updates(wallet_str, resume_ms)
                .await?;
            for update in ledger_updates.iter().take(remaining) {
                let raw = serde_json::to_value(update)?;
                records.push(RawTransaction {
                    id: Uuid::new_v4(),
                    network: network.clone(),
                    tx_hash: update.hash.clone(),
                    timestamp: unix_ms_to_secs_i64(update.time),
                    block_number: None,
                    raw_metadata: serde_json::json!({ "type": "ledger_update", "data": raw }),
                    source: "hl-rest-wallet-backfill".to_string(),
                    ingestion_run_id: None,
                    ingested_at: Utc::now(),
                });
            }
        }

        info!(
            "Hyperliquid V2 wallet backfill: collected {} records for {}",
            records.len(),
            wallet_str
        );

        Ok(IngestionBatch {
            records,
            checkpoint: None,
            run_metadata: None,
        })
    }

    /// Market backfill: fetch funding rate history for a coin, producing
    /// target-agnostic `RawTransaction` records.
    ///
    /// Hyperliquid does not expose a `recentTrades` REST endpoint, so the
    /// primary market-level backfill data source is `fundingHistory`. The WS
    /// `trades` channel provides real-time market trade data for streaming.
    async fn market_backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        let coin = target
            .address
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("market target must have an address (coin symbol)"))?;

        let start_time_ms = cursor_to_start_time_ms(cursor);
        let network = &target.network;

        // Parse filter spec for data_types if present
        let _filter: MarketFilterSpec = match &target.filter_spec {
            Some(v) => serde_json::from_value(v.clone())?,
            None => MarketFilterSpec::default(),
        };

        let rates = self
            .fetch_funding_history(coin, start_time_ms, None)
            .await?;

        let records: Vec<RawTransaction> = rates
            .iter()
            .take(limit)
            .map(|rate| {
                let raw = serde_json::to_value(rate).unwrap_or_default();
                let hash = format!("funding-rate-{}-{}", rate.coin, rate.time);
                RawTransaction {
                    id: Uuid::new_v4(),
                    network: network.clone(),
                    tx_hash: hash,
                    timestamp: unix_ms_to_secs_i64(rate.time),
                    block_number: None,
                    raw_metadata: serde_json::json!({ "type": "funding_rate", "data": raw }),
                    source: "hl-rest-market-backfill".to_string(),
                    ingestion_run_id: None,
                    ingested_at: Utc::now(),
                }
            })
            .collect();

        info!(
            "Hyperliquid V2 market backfill: collected {} records for {}",
            records.len(),
            coin
        );

        Ok(IngestionBatch {
            records,
            checkpoint: None,
            run_metadata: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Cursor helpers
// ---------------------------------------------------------------------------

/// Extract start time in milliseconds from a V2 checkpoint cursor.
///
/// Cursor shape: `{"last_time_ms": i64}`.
fn cursor_to_start_time_ms(cursor: Option<&serde_json::Value>) -> i64 {
    cursor
        .and_then(|c| c.get("last_time_ms"))
        .and_then(|v| v.as_i64())
        .map(resume_after_ms)
        .unwrap_or(0)
}

fn resume_after_ms(last_time_ms: i64) -> i64 {
    last_time_ms.saturating_add(1).max(0)
}

fn timestamp_secs_to_resume_ms(last_timestamp_secs: i64) -> i64 {
    last_timestamp_secs
        .saturating_mul(1000)
        .saturating_add(1)
        .max(0)
}

fn i64_to_u64_or_zero(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn unix_ms_to_secs_i64(time_ms: u64) -> i64 {
    i64::try_from(time_ms / 1000).unwrap_or(i64::MAX)
}

/// Source string for V2 Hyperliquid ingestion paths.
fn v2_source(target_kind: TargetKind, mode: &str) -> String {
    let kind_str = match target_kind {
        TargetKind::Wallet => "wallet",
        TargetKind::Market => "market",
        _ => "unknown",
    };
    format!("hl-ws-{kind_str}-{mode}")
}

// ---------------------------------------------------------------------------
// V2 Connector implementation
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl spectraplex_core::connector::Connector for HyperliquidAdapter {
    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supported_target_kinds: vec![TargetKind::Wallet, TargetKind::Market],
            supported_modes: vec![TargetMode::Backfill, TargetMode::Stream],
            chain_family: ChainFamily::Hyperliquid,
        }
    }

    async fn backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        if target.chain_family != ChainFamily::Hyperliquid {
            anyhow::bail!(
                "HyperliquidAdapter only supports Hyperliquid chain family, got {:?}",
                target.chain_family
            );
        }

        match target.kind {
            TargetKind::Wallet => self.wallet_backfill(target, cursor, limit).await,
            TargetKind::Market => self.market_backfill(target, cursor, limit).await,
            other => anyhow::bail!(
                "HyperliquidAdapter does not support target kind {:?}; supported: wallet, market",
                other
            ),
        }
    }

    async fn stream(
        &self,
        target: &IndexTarget,
        _cursor: Option<&serde_json::Value>,
    ) -> anyhow::Result<mpsc::Receiver<RawTransaction>> {
        if target.chain_family != ChainFamily::Hyperliquid {
            anyhow::bail!(
                "HyperliquidAdapter only supports Hyperliquid chain family, got {:?}",
                target.chain_family
            );
        }

        match target.kind {
            TargetKind::Wallet => {
                let wallet_str = target
                    .address
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("wallet target must have an address"))?;

                let (tx, rx) = mpsc::channel(256);
                let ws_client = match &self.ws_url {
                    Some(url) => HyperliquidWsClient::with_url(url),
                    None => HyperliquidWsClient::new(),
                };
                let wallet = wallet_str.to_string();
                let network = target.network.clone();
                let source = v2_source(target.kind, "stream");

                tokio::spawn(async move {
                    let result = ws_client
                        .subscribe_user(&wallet, |msg| {
                            if let Some(data) = &msg.data {
                                let channel = msg.channel.as_deref().unwrap_or("unknown");
                                let event_type = match channel {
                                    "userFills" => "fill",
                                    "userFundings" => "funding",
                                    "userNonFundingLedgerUpdates" => "ledger_update",
                                    _ => "unknown",
                                };

                                let raw_tx = RawTransaction {
                                    id: Uuid::new_v4(),
                                    network: network.clone(),
                                    tx_hash: format!("ws-{}-{}", channel, Uuid::new_v4()),
                                    timestamp: Utc::now().timestamp(),
                                    block_number: None,
                                    raw_metadata: serde_json::json!({
                                        "type": event_type,
                                        "data": data,
                                    }),
                                    source: source.clone(),
                                    ingestion_run_id: None,
                                    ingested_at: Utc::now(),
                                };

                                // Best-effort send; if receiver is dropped we
                                // will detect it when subscribe_user returns.
                                let _ = tx.try_send(raw_tx);
                            }
                        })
                        .await;

                    if let Err(e) = result {
                        tracing::error!("Hyperliquid wallet WS stream error: {}", e);
                    }
                });

                Ok(rx)
            }
            TargetKind::Market => {
                let coin = target
                    .address
                    .as_deref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("market target must have an address (coin symbol)")
                    })?;

                let (tx, rx) = mpsc::channel(256);
                let ws_client = match &self.ws_url {
                    Some(url) => HyperliquidWsClient::with_url(url),
                    None => HyperliquidWsClient::new(),
                };
                let coin_owned = coin.to_string();
                let network = target.network.clone();
                let source = v2_source(target.kind, "stream");

                tokio::spawn(async move {
                    let result = ws_client
                        .subscribe_trades(&coin_owned, |msg| {
                            if let Some(data) = &msg.data {
                                let raw_tx = RawTransaction {
                                    id: Uuid::new_v4(),
                                    network: network.clone(),
                                    tx_hash: format!("ws-trades-{}", Uuid::new_v4()),
                                    timestamp: Utc::now().timestamp(),
                                    block_number: None,
                                    raw_metadata: serde_json::json!({
                                        "type": "trade",
                                        "data": data,
                                    }),
                                    source: source.clone(),
                                    ingestion_run_id: None,
                                    ingested_at: Utc::now(),
                                };
                                let _ = tx.try_send(raw_tx);
                            }
                        })
                        .await;

                    if let Err(e) = result {
                        tracing::error!("Hyperliquid market WS stream error: {}", e);
                    }
                });

                Ok(rx)
            }
            other => anyhow::bail!(
                "HyperliquidAdapter does not support streaming for target kind {:?}; supported: wallet, market",
                other
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// V1 Legacy ChainIngestor implementation (preserved for backward compat)
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl ChainIngestor for HyperliquidAdapter {
    async fn fetch_history(
        &self,
        wallet: &str,
        limit: usize,
        user_id: Uuid,
        checkpoint: Option<&IndexerCheckpoint>,
    ) -> anyhow::Result<Vec<Transaction>> {
        let mut transactions = Vec::new();

        // Resume timestamp in milliseconds (HL API uses ms).
        // Add 1ms to avoid re-fetching the last seen event.
        let resume_ms = checkpoint
            .and_then(|cp| cp.last_timestamp)
            .map(timestamp_secs_to_resume_ms)
            .unwrap_or(0);
        let resume_ms_u64 = i64_to_u64_or_zero(resume_ms);

        // 1. Fetch fills (trades) — the fills endpoint has no startTime param,
        //    so we filter client-side.
        let fills = self.fetch_user_fills(wallet).await?;
        for fill in fills.iter().filter(|f| f.time >= resume_ms_u64).take(limit) {
            let raw = serde_json::to_value(fill)?;
            transactions.push(Transaction {
                id: Uuid::new_v4(),
                user_id,
                wallet_address: wallet.to_string(),
                timestamp: unix_ms_to_secs_i64(fill.time),
                tx_hash: fill.hash.clone(),
                chain: Chain::Hyperliquid,
                raw_metadata: serde_json::json!({ "type": "fill", "data": raw }),
            });
        }

        // 2. Fetch funding payments
        let funding = self.fetch_user_funding(wallet, resume_ms).await?;
        for entry in funding.iter().take(limit) {
            let raw = serde_json::to_value(entry)?;
            let hash = entry
                .hash
                .clone()
                .unwrap_or_else(|| format!("funding-{}-{}", entry.coin, entry.time));
            transactions.push(Transaction {
                id: Uuid::new_v4(),
                user_id,
                wallet_address: wallet.to_string(),
                timestamp: unix_ms_to_secs_i64(entry.time),
                tx_hash: hash,
                chain: Chain::Hyperliquid,
                raw_metadata: serde_json::json!({ "type": "funding", "data": raw }),
            });
        }

        // 3. Fetch non-funding ledger updates (deposits, withdrawals, liquidations)
        let ledger_updates = self.fetch_user_ledger_updates(wallet, resume_ms).await?;
        for update in ledger_updates.iter().take(limit) {
            let raw = serde_json::to_value(update)?;
            transactions.push(Transaction {
                id: Uuid::new_v4(),
                user_id,
                wallet_address: wallet.to_string(),
                timestamp: unix_ms_to_secs_i64(update.time),
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
    use spectraplex_core::connector::Connector;

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

    fn sample_funding_history_json() -> &'static str {
        r#"[
            {
                "coin": "ETH",
                "fundingRate": "0.0001",
                "premium": "0.00005",
                "time": 1700000000000
            },
            {
                "coin": "ETH",
                "fundingRate": "0.00012",
                "premium": "0.00006",
                "time": 1700003600000
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
        let funding: Vec<HlFundingEntry> = serde_json::from_str(sample_funding_json()).unwrap();
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

    #[test]
    fn test_deserialize_funding_history() {
        let rates: Vec<HlFundingRate> =
            serde_json::from_str(sample_funding_history_json()).unwrap();
        assert_eq!(rates.len(), 2);
        assert_eq!(rates[0].coin, "ETH");
        assert_eq!(rates[0].funding_rate, "0.0001");
        assert_eq!(rates[0].premium, "0.00005");
        assert_eq!(rates[1].time, 1700003600000);
    }

    // -----------------------------------------------------------------------
    // V2 Connector capability tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_capabilities_target_kinds() {
        let adapter = HyperliquidAdapter::new();
        let caps = adapter.capabilities();
        assert_eq!(caps.chain_family, ChainFamily::Hyperliquid);
        assert!(caps.supports_target_kind(TargetKind::Wallet));
        assert!(caps.supports_target_kind(TargetKind::Market));
        assert!(!caps.supports_target_kind(TargetKind::Contract));
        assert!(!caps.supports_target_kind(TargetKind::Program));
        assert!(!caps.supports_target_kind(TargetKind::Account));
        assert!(!caps.supports_target_kind(TargetKind::TopicFilter));
        assert!(!caps.supports_target_kind(TargetKind::Pool));
        assert!(!caps.supports_target_kind(TargetKind::Protocol));
    }

    #[test]
    fn test_capabilities_modes() {
        let adapter = HyperliquidAdapter::new();
        let caps = adapter.capabilities();
        assert!(caps.supports_mode(TargetMode::Backfill));
        assert!(caps.supports_mode(TargetMode::Stream));
    }

    #[test]
    fn test_capabilities_can_service_wallet() {
        let adapter = HyperliquidAdapter::new();
        let caps = adapter.capabilities();
        let target = make_target(
            TargetKind::Wallet,
            ChainFamily::Hyperliquid,
            Some("0xabc"),
            None,
        );
        assert!(caps.can_service(&target));
    }

    #[test]
    fn test_capabilities_can_service_market() {
        let adapter = HyperliquidAdapter::new();
        let caps = adapter.capabilities();
        let target = make_target(
            TargetKind::Market,
            ChainFamily::Hyperliquid,
            Some("ETH"),
            None,
        );
        assert!(caps.can_service(&target));
    }

    #[test]
    fn test_capabilities_rejects_wrong_chain_family() {
        let adapter = HyperliquidAdapter::new();
        let caps = adapter.capabilities();
        let target = make_target(TargetKind::Wallet, ChainFamily::Evm, Some("0xabc"), None);
        assert!(!caps.can_service(&target));
    }

    #[test]
    fn test_capabilities_rejects_unsupported_kind() {
        let adapter = HyperliquidAdapter::new();
        let caps = adapter.capabilities();
        let target = make_target(
            TargetKind::Contract,
            ChainFamily::Hyperliquid,
            Some("0xabc"),
            None,
        );
        assert!(!caps.can_service(&target));
    }

    // -----------------------------------------------------------------------
    // V2 backfill error handling tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_backfill_rejects_wrong_chain_family() {
        let adapter = HyperliquidAdapter::new();
        let target = make_target(TargetKind::Wallet, ChainFamily::Evm, Some("0xabc"), None);
        let result = adapter.backfill(&target, None, 10).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("only supports Hyperliquid"));
    }

    #[tokio::test]
    async fn test_backfill_rejects_unsupported_target_kind() {
        let adapter = HyperliquidAdapter::new();
        let target = make_target(
            TargetKind::Contract,
            ChainFamily::Hyperliquid,
            Some("0xabc"),
            None,
        );
        let result = adapter.backfill(&target, None, 10).await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("does not support target kind"));
        assert!(err_msg.contains("Contract"));
    }

    #[tokio::test]
    async fn test_backfill_rejects_program_target() {
        let adapter = HyperliquidAdapter::new();
        let target = make_target(
            TargetKind::Program,
            ChainFamily::Hyperliquid,
            Some("abc"),
            None,
        );
        let result = adapter.backfill(&target, None, 10).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not support target kind"));
    }

    // -----------------------------------------------------------------------
    // V2 wallet backfill with mock server
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_v2_wallet_backfill_with_mock_server() {
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
        let target = make_target(
            TargetKind::Wallet,
            ChainFamily::Hyperliquid,
            Some("0x1234567890abcdef1234567890abcdef12345678"),
            None,
        );

        let batch = adapter.backfill(&target, None, 100).await.unwrap();
        assert_eq!(batch.records.len(), 3);

        // Verify all records are RawTransaction with no wallet_address/user_id
        for record in &batch.records {
            let json = serde_json::to_string(record).unwrap();
            assert!(!json.contains("wallet_address"));
            assert!(!json.contains("user_id"));
            assert_eq!(record.network, "hypercore-mainnet");
            assert_eq!(record.source, "hl-rest-wallet-backfill");
        }

        // Verify record types
        assert_eq!(batch.records[0].raw_metadata["type"], "fill");
        assert_eq!(batch.records[0].tx_hash, "0xtest1");
        assert_eq!(batch.records[1].raw_metadata["type"], "funding");
        assert_eq!(batch.records[2].raw_metadata["type"], "ledger_update");

        server.abort();
    }

    // -----------------------------------------------------------------------
    // V2 wallet backfill with cursor/checkpoint resume
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_v2_wallet_backfill_with_cursor_resume() {
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

                    let response_body = if request.contains("userFills") {
                        r#"[{"coin":"BTC","px":"40000.0","sz":"0.1","side":"A","time":1700000000000,"hash":"0xold"}]"#
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
        let target = make_target(
            TargetKind::Wallet,
            ChainFamily::Hyperliquid,
            Some("0x1234567890abcdef1234567890abcdef12345678"),
            None,
        );

        // Cursor at the same time as the mock fill (1700000000000ms)
        // cursor_to_start_time_ms adds 1, so resume_ms = 1700000000001 > fill time
        let cursor = serde_json::json!({ "last_time_ms": 1700000000000_i64 });
        let batch = adapter.backfill(&target, Some(&cursor), 100).await.unwrap();

        // Fill should be filtered out
        assert_eq!(batch.records.len(), 0);

        server.abort();
    }

    // -----------------------------------------------------------------------
    // V2 market backfill with mock server
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_v2_market_backfill_with_mock_server() {
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

                    let response_body = if request.contains("fundingHistory") {
                        r#"[{"coin":"ETH","fundingRate":"0.0001","premium":"0.00005","time":1700000000000},{"coin":"ETH","fundingRate":"0.00012","premium":"0.00006","time":1700003600000}]"#
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
        let target = make_target(
            TargetKind::Market,
            ChainFamily::Hyperliquid,
            Some("ETH"),
            None,
        );

        let batch = adapter.backfill(&target, None, 100).await.unwrap();
        assert_eq!(batch.records.len(), 2);

        // Verify records are target-agnostic
        for record in &batch.records {
            let json = serde_json::to_string(record).unwrap();
            assert!(!json.contains("wallet_address"));
            assert!(!json.contains("user_id"));
            assert_eq!(record.network, "hypercore-mainnet");
            assert_eq!(record.source, "hl-rest-market-backfill");
            assert_eq!(record.raw_metadata["type"], "funding_rate");
            assert_eq!(record.block_number, None);
        }

        // Verify data content
        assert_eq!(
            batch.records[0].raw_metadata["data"]["fundingRate"],
            "0.0001"
        );
        assert!(batch.records[0].tx_hash.starts_with("funding-rate-ETH-"));

        server.abort();
    }

    // -----------------------------------------------------------------------
    // V2 market backfill with limit
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_v2_market_backfill_respects_limit() {
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

                    let response_body = if request.contains("fundingHistory") {
                        r#"[{"coin":"ETH","fundingRate":"0.0001","premium":"0.00005","time":1700000000000},{"coin":"ETH","fundingRate":"0.00012","premium":"0.00006","time":1700003600000}]"#
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
        let target = make_target(
            TargetKind::Market,
            ChainFamily::Hyperliquid,
            Some("ETH"),
            None,
        );

        // Limit to 1 record
        let batch = adapter.backfill(&target, None, 1).await.unwrap();
        assert_eq!(batch.records.len(), 1);

        server.abort();
    }

    // -----------------------------------------------------------------------
    // V2 stream error tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_stream_rejects_wrong_chain_family() {
        let adapter = HyperliquidAdapter::new();
        let target = make_target(TargetKind::Wallet, ChainFamily::Evm, Some("0xabc"), None);
        let result = adapter.stream(&target, None).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("only supports Hyperliquid"));
    }

    #[tokio::test]
    async fn test_stream_rejects_unsupported_target_kind() {
        let adapter = HyperliquidAdapter::new();
        let target = make_target(
            TargetKind::Contract,
            ChainFamily::Hyperliquid,
            Some("0xabc"),
            None,
        );
        let result = adapter.stream(&target, None).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not support streaming"));
    }

    // -----------------------------------------------------------------------
    // V1 Legacy ChainIngestor tests (preserved byte-identical behavior)
    // -----------------------------------------------------------------------

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
            .fetch_history(
                "0x1234567890abcdef1234567890abcdef12345678",
                10,
                Uuid::new_v4(),
                None,
            )
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

    #[tokio::test]
    async fn test_fetch_history_with_checkpoint_filters_old_data() {
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

                    let response_body = if request.contains("userFills") {
                        r#"[{"coin":"BTC","px":"40000.0","sz":"0.1","side":"A","time":1700000000000,"hash":"0xold"}]"#
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

        let checkpoint = IndexerCheckpoint {
            chain: Chain::Hyperliquid,
            wallet_address: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
            last_signature: None,
            last_slot: None,
            last_block: None,
            last_timestamp: Some(1700000000), // same second as mock data
        };

        let adapter = HyperliquidAdapter::with_base_url(&base_url);
        let txs = adapter
            .fetch_history(
                "0x1234567890abcdef1234567890abcdef12345678",
                10,
                Uuid::new_v4(),
                Some(&checkpoint),
            )
            .await
            .unwrap();

        // Fill at time=1700000000000ms should be filtered out because
        // resume_ms = (1700000000 * 1000) + 1 = 1700000000001 > 1700000000000
        assert_eq!(txs.len(), 0);

        server.abort();
    }

    #[tokio::test]
    async fn test_client_timeout_fires() {
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
                    use tokio::io::AsyncReadExt;
                    let mut stream = stream;
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    // Never respond -- force a timeout
                    tokio::time::sleep(Duration::from_secs(120)).await;
                });
            }
        });

        // Build a client with a short timeout to keep the test fast
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let adapter = HyperliquidAdapter {
            client,
            base_url,
            ws_url: None,
        };

        let result = adapter.fetch_user_fills("0xtest").await;
        assert!(result.is_err());
        let err_chain = format!("{:#}", result.unwrap_err()).to_lowercase();
        assert!(
            err_chain.contains("timed out")
                || err_chain.contains("timeout")
                || err_chain.contains("deadline has elapsed"),
            "expected timeout error, got: {}",
            err_chain
        );

        server.abort();
    }

    #[tokio::test]
    async fn test_non_success_response_empty_body_is_descriptive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{}", addr);

        let server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut stream = stream;
                    let mut buf = vec![0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let response = "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });

        let adapter = HyperliquidAdapter::with_base_url(&base_url);
        let result = adapter.fetch_user_fills("0xtest").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Hyperliquid userFills returned 429"));
        assert!(err.contains("<empty response body>"));

        server.abort();
    }

    // -----------------------------------------------------------------------
    // Cursor / checkpoint helpers
    // -----------------------------------------------------------------------

    #[test]
    fn test_cursor_to_start_time_ms_none() {
        assert_eq!(cursor_to_start_time_ms(None), 0);
    }

    #[test]
    fn test_cursor_to_start_time_ms_with_value() {
        let cursor = serde_json::json!({ "last_time_ms": 1700000000000_i64 });
        // Should advance by 1ms past last seen
        assert_eq!(cursor_to_start_time_ms(Some(&cursor)), 1700000000001);
    }

    #[test]
    fn test_cursor_to_start_time_ms_saturates_max_value() {
        let cursor = serde_json::json!({ "last_time_ms": i64::MAX });
        assert_eq!(cursor_to_start_time_ms(Some(&cursor)), i64::MAX);
    }

    #[test]
    fn test_cursor_to_start_time_ms_clamps_negative_value() {
        let cursor = serde_json::json!({ "last_time_ms": -5_i64 });
        assert_eq!(cursor_to_start_time_ms(Some(&cursor)), 0);
    }

    #[test]
    fn test_cursor_to_start_time_ms_missing_key() {
        let cursor = serde_json::json!({ "other_key": 123 });
        assert_eq!(cursor_to_start_time_ms(Some(&cursor)), 0);
    }

    #[test]
    fn test_timestamp_secs_to_resume_ms_boundaries() {
        assert_eq!(
            timestamp_secs_to_resume_ms(1_700_000_000),
            1_700_000_000_001
        );
        assert_eq!(timestamp_secs_to_resume_ms(-5), 0);
        assert_eq!(timestamp_secs_to_resume_ms(i64::MAX), i64::MAX);
    }

    #[test]
    fn test_i64_to_u64_or_zero() {
        assert_eq!(i64_to_u64_or_zero(42), 42);
        assert_eq!(i64_to_u64_or_zero(-1), 0);
    }

    #[test]
    fn test_unix_ms_to_secs_i64_boundaries() {
        assert_eq!(unix_ms_to_secs_i64(1_700_000_000_999), 1_700_000_000);
        assert_eq!(unix_ms_to_secs_i64(u64::MAX), 18_446_744_073_709_551);
    }

    #[test]
    fn test_v2_source_strings() {
        assert_eq!(
            v2_source(TargetKind::Wallet, "stream"),
            "hl-ws-wallet-stream"
        );
        assert_eq!(
            v2_source(TargetKind::Market, "stream"),
            "hl-ws-market-stream"
        );
        assert_eq!(
            v2_source(TargetKind::Wallet, "backfill"),
            "hl-ws-wallet-backfill"
        );
        assert_eq!(
            v2_source(TargetKind::Contract, "stream"),
            "hl-ws-unknown-stream"
        );
    }

    // -----------------------------------------------------------------------
    // RawTransaction output verification
    // -----------------------------------------------------------------------

    #[test]
    fn test_raw_transaction_has_no_wallet_or_user_fields() {
        let rt = RawTransaction {
            id: Uuid::new_v4(),
            network: "hypercore-mainnet".to_string(),
            tx_hash: "0xtest".to_string(),
            timestamp: 1700000000,
            block_number: None,
            raw_metadata: serde_json::json!({"type": "fill"}),
            source: "hl-rest-wallet-backfill".to_string(),
            ingestion_run_id: None,
            ingested_at: Utc::now(),
        };
        let json = serde_json::to_string(&rt).unwrap();
        assert!(!json.contains("wallet_address"));
        assert!(!json.contains("user_id"));
    }

    // -----------------------------------------------------------------------
    // Helper
    // -----------------------------------------------------------------------

    fn make_target(
        kind: TargetKind,
        chain_family: ChainFamily,
        address: Option<&str>,
        filter_spec: Option<serde_json::Value>,
    ) -> IndexTarget {
        let now = Utc::now();
        IndexTarget {
            id: Uuid::new_v4(),
            kind,
            network: "hypercore-mainnet".to_string(),
            chain_family,
            address: address.map(|s| s.to_string()),
            filter_spec,
            mode: TargetMode::Both,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    // -- Construction from NetworkContext --

    #[test]
    fn test_from_network_context_with_both_providers() {
        use spectraplex_core::config::{NetworkConfig, ProviderConfig};
        use spectraplex_core::provider::{NetworkId, ProviderRegistry};
        use std::collections::HashMap;

        let mut networks = HashMap::new();
        networks.insert(
            "hypercore-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );

        let providers = vec![
            ProviderConfig {
                network: "hypercore-mainnet".to_string(),
                kind: "rest".to_string(),
                url: "https://api.hyperliquid.xyz".to_string(),
                priority: Some(1),
                capabilities: vec!["historical".to_string()],
                token_env: None,
                token: None,
                headers: None,
            },
            ProviderConfig {
                network: "hypercore-mainnet".to_string(),
                kind: "ws".to_string(),
                url: "wss://api.hyperliquid.xyz/ws".to_string(),
                priority: Some(1),
                capabilities: vec!["stream".to_string()],
                token_env: None,
                token: None,
                headers: None,
            },
        ];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("hypercore-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &net).unwrap();
        let adapter = HyperliquidAdapter::from_network_context(&ctx);

        assert_eq!(adapter.base_url, "https://api.hyperliquid.xyz/info");
        assert_eq!(
            adapter.ws_url.as_deref(),
            Some("wss://api.hyperliquid.xyz/ws")
        );
    }

    #[test]
    fn test_from_network_context_defaults_without_providers() {
        use spectraplex_core::config::NetworkConfig;
        use spectraplex_core::provider::{NetworkId, ProviderRegistry};
        use std::collections::HashMap;

        let mut networks = HashMap::new();
        networks.insert(
            "hypercore-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );

        // No providers for this network
        let registry = ProviderRegistry::from_config(&networks, &[]).unwrap();
        let net = NetworkId::new("hypercore-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &net).unwrap();
        let adapter = HyperliquidAdapter::from_network_context(&ctx);

        // Falls back to hardcoded defaults
        assert_eq!(adapter.base_url, HL_INFO_URL);
        assert!(adapter.ws_url.is_none());
    }

    #[test]
    fn test_from_network_context_url_ending_with_info() {
        use spectraplex_core::config::{NetworkConfig, ProviderConfig};
        use spectraplex_core::provider::{NetworkId, ProviderRegistry};
        use std::collections::HashMap;

        let mut networks = HashMap::new();
        networks.insert(
            "hypercore-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );

        let providers = vec![ProviderConfig {
            network: "hypercore-mainnet".to_string(),
            kind: "rest".to_string(),
            url: "https://api.hyperliquid.xyz/info".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("hypercore-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &net).unwrap();
        let adapter = HyperliquidAdapter::from_network_context(&ctx);

        // Should not double-append /info
        assert_eq!(adapter.base_url, "https://api.hyperliquid.xyz/info");
    }
}
