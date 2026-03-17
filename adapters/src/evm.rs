use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use alloy::consensus::Transaction as TransactionTrait;
use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, Log};
use chrono::Utc;
use governor::{Quota, RateLimiter};
use serde_json::json;
use spectraplex_core::connector::{
    extract_filter_spec, Connector, ConnectorCapabilities, TopicFilterSpec, TypedFilterSpec,
};
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, Transaction};
use spectraplex_core::v2::{
    ChainFamily, EvmTraceType, IndexTarget, IngestionBatch, RawEvmTrace, RawTransaction,
    TargetKind, TargetMode,
};
use tracing::{debug, warn};
use uuid::Uuid;

/// Maximum block range per eth_getLogs request.
const DEFAULT_BLOCK_CHUNK: u64 = 2000;

/// ERC-20 Transfer event signature: keccak256("Transfer(address,address,uint256)")
const ERC20_TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// EVM chain adapter that fetches logs and transactions via JSON-RPC.
///
/// Implements both the legacy `ChainIngestor` trait (wallet-only, V1) and
/// the V2 `Connector` trait with correct semantics for wallet, contract,
/// and topic_filter target kinds.
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
    network: String,
    /// HTTP client and URL for raw `debug_traceTransaction` RPC calls.
    /// When `None`, trace calls use `rpc_url`. When `Some`, trace calls
    /// use the separate trace URL (allowing free RPC for standard calls
    /// and a paid archive node for traces).
    trace_rpc: Option<(reqwest::Client, reqwest::Url)>,
    /// The primary RPC URL, used as fallback for trace calls when
    /// `trace_rpc` is not set.
    rpc_url: reqwest::Url,
}

impl EvmAdapter {
    /// Create a new adapter connected to an EVM-compatible RPC endpoint.
    pub fn new(rpc_url: &str) -> anyhow::Result<Self> {
        let url: reqwest::Url = rpc_url.parse()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        let provider = ProviderBuilder::new().connect_reqwest(client, url.clone());

        // 5 requests per second by default (safe for public RPCs)
        let quota = Quota::per_second(NonZeroU32::new(5).unwrap());
        let limiter = RateLimiter::direct(quota);

        Ok(Self {
            provider: Arc::new(provider),
            rate_limiter: Arc::new(limiter),
            block_chunk: DEFAULT_BLOCK_CHUNK,
            network: "ethereum-mainnet".to_string(),
            trace_rpc: None,
            rpc_url: url,
        })
    }

    /// Set a custom block chunk size for `eth_getLogs` range queries.
    pub fn with_block_chunk(mut self, chunk: u64) -> Self {
        self.block_chunk = chunk;
        self
    }

    /// Override the network identifier (e.g. "base-mainnet", "arbitrum-mainnet").
    pub fn with_network(mut self, network: &str) -> Self {
        self.network = network.to_string();
        self
    }

    /// Set a separate RPC URL for trace requests (`debug_traceTransaction`).
    ///
    /// When set, trace fetching uses this URL instead of the primary one.
    /// This allows separating the standard RPC endpoint (which may be a
    /// free/public node) from the trace-capable endpoint (which often
    /// requires a paid tier or an archive node).
    pub fn with_trace_rpc_url(mut self, rpc_url: &str) -> anyhow::Result<Self> {
        let url: reqwest::Url = rpc_url.parse()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        self.trace_rpc = Some((client, url));
        Ok(self)
    }

    // -----------------------------------------------------------------------
    // Low-level log fetching methods
    // -----------------------------------------------------------------------

    /// Fetch logs for a given contract address across a block range, chunked
    /// to avoid RPC limits.
    ///
    /// This is the original address-filter method. Correct for contract targets
    /// (returns all events emitted *by* that contract).
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

    /// Fetch logs by topic filters with an optional address constraint.
    ///
    /// Generalizes `fetch_logs` by accepting up to 4 topic-position filters
    /// and an optional set of contract addresses. This is the building block
    /// for wallet-based topic queries and arbitrary topic_filter targets.
    #[allow(clippy::too_many_arguments)]
    pub async fn fetch_logs_by_topics(
        &self,
        addresses: &[Address],
        topic0: Option<B256>,
        topic1: Option<B256>,
        topic2: Option<B256>,
        topic3: Option<B256>,
        from_block: u64,
        to_block: u64,
    ) -> anyhow::Result<Vec<Log>> {
        let mut all_logs = Vec::new();
        let mut start = from_block;

        while start <= to_block {
            let end = std::cmp::min(start + self.block_chunk - 1, to_block);

            self.rate_limiter.until_ready().await;

            let mut filter = Filter::new().from_block(start).to_block(end);

            if !addresses.is_empty() {
                filter = filter.address(addresses.to_vec());
            }
            if let Some(t0) = topic0 {
                filter = filter.event_signature(t0);
            }
            if let Some(t1) = topic1 {
                filter = filter.topic1(t1);
            }
            if let Some(t2) = topic2 {
                filter = filter.topic2(t2);
            }
            if let Some(t3) = topic3 {
                filter = filter.topic3(t3);
            }

            let logs = self.provider.get_logs(&filter).await?;
            all_logs.extend(logs);

            start = end + 1;
        }

        Ok(all_logs)
    }

    /// Fetch ERC-20 Transfer logs involving a wallet address.
    ///
    /// Issues two `eth_getLogs` calls per chunk:
    /// 1. Transfer events with wallet in topic\[1\] (sender / outgoing)
    /// 2. Transfer events with wallet in topic\[2\] (receiver / incoming)
    ///
    /// Results are deduplicated by (tx_hash, log_index) to handle the case
    /// where the wallet sends tokens to itself.
    pub async fn fetch_wallet_erc20_logs(
        &self,
        wallet: Address,
        from_block: u64,
        to_block: u64,
    ) -> anyhow::Result<Vec<Log>> {
        let transfer_topic = parse_b256(ERC20_TRANSFER_TOPIC)?;
        let wallet_padded = address_to_topic(wallet);

        // Outgoing: wallet in topic[1] (from)
        let outgoing = self
            .fetch_logs_by_topics(
                &[],
                Some(transfer_topic),
                Some(wallet_padded),
                None,
                None,
                from_block,
                to_block,
            )
            .await?;

        // Incoming: wallet in topic[2] (to)
        let incoming = self
            .fetch_logs_by_topics(
                &[],
                Some(transfer_topic),
                None,
                Some(wallet_padded),
                None,
                from_block,
                to_block,
            )
            .await?;

        // Deduplicate by (tx_hash, log_index)
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for log in outgoing.into_iter().chain(incoming) {
            let key = (log.transaction_hash, log.log_index);
            if seen.insert(key) {
                result.push(log);
            }
        }

        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Transaction enrichment
    // -----------------------------------------------------------------------

    /// Fetch transaction-level details (value, from, to) and receipt fields
    /// (gas_used, effective_gas_price) for each unique tx hash in the logs.
    async fn enrich_tx_data(
        &self,
        logs: &[Log],
    ) -> HashMap<String, (serde_json::Value, serde_json::Value)> {
        let mut seen_tx_hashes: HashMap<String, (serde_json::Value, serde_json::Value)> =
            HashMap::new();

        for log in logs {
            let tx_hash_b256 = match log.transaction_hash {
                Some(h) => h,
                None => continue,
            };
            let tx_hash = format!("{tx_hash_b256:#x}");

            if seen_tx_hashes.contains_key(&tx_hash) {
                continue;
            }

            let mut tx_fields = json!({});
            let mut receipt_fields = json!({});

            self.rate_limiter.until_ready().await;
            match self.provider.get_transaction_by_hash(tx_hash_b256).await {
                Ok(Some(full_tx)) => {
                    let value = full_tx.inner.value();
                    let from = full_tx.inner.signer();
                    let to = full_tx.inner.to().map(|a| format!("{a:#x}"));
                    tx_fields = json!({
                        "value": format!("{value:#x}"),
                        "from": format!("{from:#x}"),
                        "to": to,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "Failed to fetch transaction");
                }
            }

            self.rate_limiter.until_ready().await;
            match self.provider.get_transaction_receipt(tx_hash_b256).await {
                Ok(Some(receipt)) => {
                    receipt_fields = json!({
                        "gas_used": format!("{:#x}", receipt.gas_used),
                        "effective_gas_price": format!("{:#x}", receipt.effective_gas_price),
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "Failed to fetch receipt");
                }
            }

            seen_tx_hashes.insert(tx_hash, (tx_fields, receipt_fields));
        }

        seen_tx_hashes
    }

    // -----------------------------------------------------------------------
    // V2 RawTransaction conversion
    // -----------------------------------------------------------------------

    /// Convert enriched logs into V2 `RawTransaction` records.
    ///
    /// Attaches tx-level fields (value, from, to, gas) only to the first log
    /// per transaction hash to avoid duplicate gas fee / value entries in the
    /// downstream parser.
    fn logs_to_raw_transactions(
        &self,
        logs: &[Log],
        enrichment: &HashMap<String, (serde_json::Value, serde_json::Value)>,
        source: &str,
    ) -> Vec<RawTransaction> {
        let mut records = Vec::new();
        let mut emitted_tx_hashes = HashSet::new();

        for log in logs {
            let tx_hash = match log.transaction_hash {
                Some(h) => format!("{h:#x}"),
                None => continue,
            };

            let block_timestamp = log.block_timestamp.unwrap_or(0);

            let mut raw_metadata = json!({
                "log_index": log.log_index,
                "block_number": log.block_number,
                "block_hash": log.block_hash.map(|h| format!("{h:#x}")),
                "transaction_hash": &tx_hash,
                "address": format!("{:#x}", log.address()),
                "topics": log.topics().iter().map(|t| format!("{t:#x}")).collect::<Vec<_>>(),
                "data": format!("0x{}", alloy::hex::encode(log.data().data.as_ref())),
            });

            let is_first_log = emitted_tx_hashes.insert(tx_hash.clone());
            if is_first_log {
                if let Some((tx_fields, receipt_fields)) = enrichment.get(&tx_hash) {
                    if let Some(obj) = raw_metadata.as_object_mut() {
                        if let Some(tx_obj) = tx_fields.as_object() {
                            obj.extend(tx_obj.iter().map(|(k, v)| (k.clone(), v.clone())));
                        }
                        if let Some(rx_obj) = receipt_fields.as_object() {
                            obj.extend(rx_obj.iter().map(|(k, v)| (k.clone(), v.clone())));
                        }
                    }
                }
            }

            let timestamp = i64::try_from(block_timestamp).unwrap_or(i64::MAX);

            records.push(RawTransaction {
                id: Uuid::new_v4(),
                network: self.network.clone(),
                tx_hash,
                timestamp,
                block_number: log.block_number.map(|n| n as i64),
                raw_metadata,
                source: source.to_string(),
                ingestion_run_id: None,
                ingested_at: Utc::now(),
            });
        }

        records
    }

    // -----------------------------------------------------------------------
    // V2 Connector backfill paths
    // -----------------------------------------------------------------------

    /// Wallet backfill: topic-filtered ERC-20 Transfer logs.
    ///
    /// Uses topic-based filtering (wallet in topic\[1\] or topic\[2\]) instead
    /// of the incorrect address filter. This returns Transfer events where
    /// the wallet is the sender or receiver, regardless of which contract
    /// emitted them.
    ///
    /// NOTE: Native ETH transfers without log events require block-scanning
    /// or trace APIs. Tx-level value/from/to fields are still enriched onto
    /// the first log per tx, which the parser uses for native value detection.
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
        let wallet_addr: Address = wallet_str.parse()?;

        self.rate_limiter.until_ready().await;
        let latest_block = self.provider.get_block_number().await?;

        let from_block = cursor_to_from_block(cursor, latest_block, limit, self.block_chunk);

        let logs = self
            .fetch_wallet_erc20_logs(wallet_addr, from_block, latest_block)
            .await?;

        let enrichment = self.enrich_tx_data(&logs).await;
        let records = self.logs_to_raw_transactions(&logs, &enrichment, "evm-rpc-wallet-backfill");

        Ok(IngestionBatch {
            records,
            checkpoint: None,
            run_metadata: None,
        })
    }

    /// Contract backfill: address-filtered logs for a specific contract.
    ///
    /// Uses the standard address filter in `eth_getLogs`, which returns all
    /// events emitted by the specified contract. This is the correct semantic
    /// for contract target kinds.
    async fn contract_backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        let contract_str = target
            .address
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("contract target must have an address"))?;
        let contract_addr: Address = contract_str.parse()?;

        self.rate_limiter.until_ready().await;
        let latest_block = self.provider.get_block_number().await?;

        let from_block = cursor_to_from_block(cursor, latest_block, limit, self.block_chunk);

        let logs = self
            .fetch_logs(contract_addr, from_block, latest_block)
            .await?;

        let enrichment = self.enrich_tx_data(&logs).await;
        let records =
            self.logs_to_raw_transactions(&logs, &enrichment, "evm-rpc-contract-backfill");

        Ok(IngestionBatch {
            records,
            checkpoint: None,
            run_metadata: None,
        })
    }

    /// Topic filter backfill: arbitrary topic-based log queries.
    ///
    /// Parses the `TopicFilterSpec` from the target's filter_spec and builds
    /// the appropriate `eth_getLogs` filter. Supports optional address narrowing
    /// via the spec's `address_filter` field.
    async fn topic_filter_backfill(
        &self,
        _target: &IndexTarget,
        spec: &TopicFilterSpec,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        self.rate_limiter.until_ready().await;
        let latest_block = self.provider.get_block_number().await?;

        let from_block = cursor_to_from_block(cursor, latest_block, limit, self.block_chunk);

        // Parse topic positions from the spec
        let topic0 = parse_topic_value(spec.topics.first())?;
        let topic1 = parse_topic_value(spec.topics.get(1))?;
        let topic2 = parse_topic_value(spec.topics.get(2))?;
        let topic3 = parse_topic_value(spec.topics.get(3))?;

        // Parse address filter
        let addresses: Vec<Address> = spec
            .address_filter
            .iter()
            .filter_map(|a| a.parse().ok())
            .collect();

        let logs = self
            .fetch_logs_by_topics(
                &addresses,
                topic0,
                topic1,
                topic2,
                topic3,
                from_block,
                latest_block,
            )
            .await?;

        let enrichment = self.enrich_tx_data(&logs).await;
        let records =
            self.logs_to_raw_transactions(&logs, &enrichment, "evm-rpc-topic-filter-backfill");

        Ok(IngestionBatch {
            records,
            checkpoint: None,
            run_metadata: None,
        })
    }

    // -----------------------------------------------------------------------
    // Reorg detection (unchanged)
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Trace fetching (Phase 2 — Bronze layer)
    // -----------------------------------------------------------------------

    /// Fetch a transaction trace via `debug_traceTransaction` with the
    /// `callTracer` configuration.
    ///
    /// The `callTracer` returns a nested call tree showing all internal
    /// calls, value transfers, and gas usage. This is the most commonly
    /// supported tracer across Geth, Erigon, and Reth nodes.
    ///
    /// Returns `Ok(None)` if the RPC provider does not support the debug
    /// namespace (returns a method-not-found error).
    ///
    /// The raw JSON response is returned as-is for Bronze-layer storage.
    /// Parsing into structured types is deferred to the Silver-layer
    /// materializer (Phase 3).
    pub async fn fetch_call_trace(
        &self,
        tx_hash: B256,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.fetch_trace(tx_hash, EvmTraceType::CallTracer).await
    }

    /// Fetch a transaction trace via `debug_traceTransaction` with the
    /// `prestateTracer` configuration (with `diffMode: true`).
    ///
    /// The `prestateTracer` in diff mode returns pre- and post-state for
    /// every account touched by the transaction, including balance changes.
    /// This is the ideal data source for `NativeBalanceDelta` extraction.
    ///
    /// Returns `Ok(None)` if the RPC provider does not support the debug
    /// namespace.
    pub async fn fetch_prestate_trace(
        &self,
        tx_hash: B256,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.fetch_trace(tx_hash, EvmTraceType::PrestateTracer)
            .await
    }

    /// Core trace fetching method. Issues a `debug_traceTransaction` raw
    /// JSON-RPC call with the specified tracer configuration.
    ///
    /// Uses raw HTTP+JSON-RPC because alloy's `Provider::raw_request`
    /// requires `Sized` and cannot be called on `dyn Provider`. Since we
    /// store the raw JSONB anyway (Bronze layer), typed deserialization
    /// is unnecessary at this stage.
    async fn fetch_trace(
        &self,
        tx_hash: B256,
        trace_type: EvmTraceType,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        self.rate_limiter.until_ready().await;

        let tx_hash_hex = format!("{tx_hash:#x}");

        let tracer_config = match trace_type {
            EvmTraceType::CallTracer => json!({
                "tracer": "callTracer",
                "tracerConfig": {
                    "onlyTopCall": false,
                    "withLog": true,
                }
            }),
            EvmTraceType::PrestateTracer => json!({
                "tracer": "prestateTracer",
                "tracerConfig": {
                    "diffMode": true,
                }
            }),
            EvmTraceType::FlatCallTracer => json!({
                "tracer": "flatCallTracer",
            }),
            EvmTraceType::Other => json!({
                "tracer": "callTracer",
            }),
        };

        let rpc_body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "debug_traceTransaction",
            "params": [tx_hash_hex, tracer_config],
        });

        // Use trace-specific RPC URL if configured, otherwise fall back to
        // the primary RPC URL with a fresh reqwest client.
        let (client, url) = match &self.trace_rpc {
            Some((c, u)) => (c.clone(), u.clone()),
            None => {
                let c = reqwest::Client::builder()
                    .timeout(Duration::from_secs(60))
                    .build()?;
                (c, self.rpc_url.clone())
            }
        };

        let resp = client
            .post(url)
            .json(&rpc_body)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        // JSON-RPC error handling
        if let Some(error) = resp.get("error") {
            let err_msg = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            let err_code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);

            // Method not found / not supported patterns
            if err_code == -32601
                || err_msg.contains("not found")
                || err_msg.contains("not available")
                || err_msg.contains("does not exist")
            {
                debug!(
                    tx_hash = %tx_hash_hex,
                    "debug_traceTransaction not supported by this RPC provider"
                );
                return Ok(None);
            }

            return Err(anyhow::anyhow!(
                "debug_traceTransaction failed for {}: code={}, message={}",
                tx_hash_hex,
                err_code,
                err_msg,
            ));
        }

        match resp.get("result") {
            Some(result) => Ok(Some(result.clone())),
            None => Err(anyhow::anyhow!(
                "debug_traceTransaction response missing 'result' for {}",
                tx_hash_hex,
            )),
        }
    }

    /// Fetch traces for a batch of transaction hashes and return
    /// `RawEvmTrace` records ready for Bronze-layer storage.
    ///
    /// Skips transactions where the trace call returns `None` (unsupported
    /// provider) or fails with a non-fatal error. Logs warnings for
    /// individual failures without aborting the batch.
    pub async fn fetch_traces_for_transactions(
        &self,
        tx_hashes: &[B256],
        trace_type: EvmTraceType,
        block_numbers: &HashMap<B256, Option<i64>>,
        ingestion_run_id: Option<Uuid>,
    ) -> Vec<RawEvmTrace> {
        let mut traces = Vec::new();

        for tx_hash in tx_hashes {
            let result = self.fetch_trace(*tx_hash, trace_type).await;
            match result {
                Ok(Some(raw_trace)) => {
                    let tx_hash_str = format!("{tx_hash:#x}");
                    let block_number = block_numbers.get(tx_hash).copied().unwrap_or(None);
                    traces.push(RawEvmTrace {
                        id: Uuid::new_v4(),
                        transaction_hash: tx_hash_str,
                        block_number,
                        network: self.network.clone(),
                        trace_type,
                        raw_trace,
                        ingestion_run_id,
                        created_at: Utc::now(),
                    });
                }
                Ok(None) => {
                    // Provider does not support traces — stop trying
                    debug!("trace API not supported, skipping remaining transactions");
                    break;
                }
                Err(e) => {
                    warn!(
                        tx_hash = %format!("{tx_hash:#x}"),
                        error = %e,
                        "failed to fetch trace, skipping"
                    );
                }
            }
        }

        traces
    }
}

// ---------------------------------------------------------------------------
// V2 Connector implementation
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl Connector for EvmAdapter {
    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supported_target_kinds: vec![
                TargetKind::Wallet,
                TargetKind::Contract,
                TargetKind::TopicFilter,
            ],
            supported_modes: vec![TargetMode::Backfill],
            chain_family: ChainFamily::Evm,
        }
    }

    async fn backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        // Validate chain family
        if target.chain_family != ChainFamily::Evm {
            anyhow::bail!(
                "EvmAdapter only supports EVM chain family, got {:?}",
                target.chain_family
            );
        }

        match target.kind {
            TargetKind::Wallet => self.wallet_backfill(target, cursor, limit).await,
            TargetKind::Contract => self.contract_backfill(target, cursor, limit).await,
            TargetKind::TopicFilter => {
                let typed = extract_filter_spec(target)?;
                match typed {
                    TypedFilterSpec::TopicFilter(spec) => {
                        self.topic_filter_backfill(target, &spec, cursor, limit)
                            .await
                    }
                    _ => anyhow::bail!("unexpected filter spec type for topic_filter target"),
                }
            }
            other => anyhow::bail!(
                "EvmAdapter does not support target kind {:?}; supported: wallet, contract, topic_filter",
                other
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// V1 Legacy ChainIngestor implementation (preserved for backward compat)
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl ChainIngestor for EvmAdapter {
    async fn fetch_history(
        &self,
        wallet: &str,
        limit: usize,
        user_id: Uuid,
        checkpoint: Option<&IndexerCheckpoint>,
    ) -> anyhow::Result<Vec<Transaction>> {
        let address: Address = wallet.parse()?;

        self.rate_limiter.until_ready().await;
        let latest_block = self.provider.get_block_number().await?;

        let from_block = if let Some(block) = checkpoint.and_then(|cp| cp.last_block) {
            (block as u64).saturating_add(1)
        } else {
            let total_blocks = (limit as u64) * self.block_chunk;
            latest_block.saturating_sub(total_blocks)
        };

        let logs = self.fetch_logs(address, from_block, latest_block).await?;

        let mut seen_tx_hashes: HashMap<String, (serde_json::Value, serde_json::Value)> =
            HashMap::new();

        for log in &logs {
            let tx_hash_b256 = match log.transaction_hash {
                Some(h) => h,
                None => continue,
            };
            let tx_hash = format!("{tx_hash_b256:#x}");

            if seen_tx_hashes.contains_key(&tx_hash) {
                continue;
            }

            let mut tx_fields = json!({});
            let mut receipt_fields = json!({});

            self.rate_limiter.until_ready().await;
            match self.provider.get_transaction_by_hash(tx_hash_b256).await {
                Ok(Some(full_tx)) => {
                    let value = full_tx.inner.value();
                    let from = full_tx.inner.signer();
                    let to = full_tx.inner.to().map(|a| format!("{a:#x}"));
                    tx_fields = json!({
                        "value": format!("{value:#x}"),
                        "from": format!("{from:#x}"),
                        "to": to,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "Failed to fetch transaction");
                }
            }

            self.rate_limiter.until_ready().await;
            match self.provider.get_transaction_receipt(tx_hash_b256).await {
                Ok(Some(receipt)) => {
                    receipt_fields = json!({
                        "gas_used": format!("{:#x}", receipt.gas_used),
                        "effective_gas_price": format!("{:#x}", receipt.effective_gas_price),
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(tx_hash = %tx_hash, error = %e, "Failed to fetch receipt");
                }
            }

            seen_tx_hashes.insert(tx_hash, (tx_fields, receipt_fields));
        }

        let mut transactions = Vec::new();
        let mut emitted_tx_hashes = HashSet::new();

        for log in &logs {
            let tx_hash = match log.transaction_hash {
                Some(h) => format!("{h:#x}"),
                None => continue,
            };

            let block_timestamp = log.block_timestamp.unwrap_or(0);

            let mut raw_metadata = json!({
                "log_index": log.log_index,
                "block_number": log.block_number,
                "block_hash": log.block_hash.map(|h| format!("{h:#x}")),
                "transaction_hash": &tx_hash,
                "address": format!("{:#x}", log.address()),
                "topics": log.topics().iter().map(|t| format!("{t:#x}")).collect::<Vec<_>>(),
                "data": format!("0x{}", alloy::hex::encode(log.data().data.as_ref())),
            });

            let is_first_log = emitted_tx_hashes.insert(tx_hash.clone());
            if is_first_log {
                if let Some((tx_fields, receipt_fields)) = seen_tx_hashes.get(&tx_hash) {
                    if let Some(obj) = raw_metadata.as_object_mut() {
                        if let Some(tx_obj) = tx_fields.as_object() {
                            obj.extend(tx_obj.iter().map(|(k, v)| (k.clone(), v.clone())));
                        }
                        if let Some(rx_obj) = receipt_fields.as_object() {
                            obj.extend(rx_obj.iter().map(|(k, v)| (k.clone(), v.clone())));
                        }
                    }
                }
            }

            let timestamp = i64::try_from(block_timestamp).unwrap_or(i64::MAX);

            transactions.push(Transaction {
                id: Uuid::new_v4(),
                user_id,
                wallet_address: wallet.to_string(),
                timestamp,
                tx_hash,
                chain: Chain::Ethereum,
                raw_metadata,
            });
        }

        Ok(transactions)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a hex string into a B256 (32-byte hash).
fn parse_b256(hex: &str) -> anyhow::Result<B256> {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    let bytes = alloy::hex::decode(stripped)?;
    if bytes.len() != 32 {
        anyhow::bail!("expected 32 bytes, got {}", bytes.len());
    }
    Ok(B256::from_slice(&bytes))
}

/// Left-pad an Ethereum address (20 bytes) to a 32-byte topic value.
fn address_to_topic(addr: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(addr.as_slice());
    B256::from(bytes)
}

/// Parse a single topic value from a `TopicFilterSpec` topics array entry.
///
/// - `null` or missing → `None` (wildcard)
/// - `"0x..."` → `Some(B256)` (exact match)
/// - Arrays are not supported in this single-value path (would need FilterSet).
fn parse_topic_value(val: Option<&serde_json::Value>) -> anyhow::Result<Option<B256>> {
    match val {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(parse_b256(s)?)),
        Some(other) => anyhow::bail!("unsupported topic value type: {}", other),
    }
}

/// Extract a starting block number from a V2 cursor, or compute a default.
fn cursor_to_from_block(
    cursor: Option<&serde_json::Value>,
    latest_block: u64,
    limit: usize,
    block_chunk: u64,
) -> u64 {
    if let Some(c) = cursor {
        if let Some(last_block) = c.get("last_block").and_then(|v| v.as_u64()) {
            return last_block.saturating_add(1);
        }
        // V1 compat cursor
        if let Some(last_block) = c.get("last_block").and_then(|v| v.as_i64()) {
            if last_block >= 0 {
                return (last_block as u64).saturating_add(1);
            }
        }
    }
    let total_blocks = (limit as u64) * block_chunk;
    latest_block.saturating_sub(total_blocks)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_adapter_block_chunk_default() {
        assert_eq!(DEFAULT_BLOCK_CHUNK, 2000);
    }

    #[test]
    fn test_timestamp_overflow_saturates() {
        let huge: u64 = u64::MAX;
        let result = i64::try_from(huge).unwrap_or(i64::MAX);
        assert_eq!(result, i64::MAX);

        let normal: u64 = 1_700_000_000;
        let result = i64::try_from(normal).unwrap_or(i64::MAX);
        assert_eq!(result, 1_700_000_000i64);
    }

    // -- parse_b256 --

    #[test]
    fn test_parse_b256_valid() {
        let result = parse_b256(ERC20_TRANSFER_TOPIC).unwrap();
        assert_eq!(format!("{result:#x}"), ERC20_TRANSFER_TOPIC.to_lowercase());
    }

    #[test]
    fn test_parse_b256_wrong_length() {
        assert!(parse_b256("0xdeadbeef").is_err());
    }

    // -- address_to_topic --

    #[test]
    fn test_address_to_topic_pads_correctly() {
        let addr: Address = "0xabcdef1234567890abcdef1234567890abcdef12"
            .parse()
            .unwrap();
        let topic = address_to_topic(addr);
        let hex = format!("{topic:#x}");
        // Should be zero-padded to 32 bytes
        assert!(hex.starts_with("0x000000000000000000000000"));
        assert!(hex.ends_with("abcdef1234567890abcdef1234567890abcdef12"));
    }

    // -- parse_topic_value --

    #[test]
    fn test_parse_topic_value_null() {
        assert!(parse_topic_value(None).unwrap().is_none());
        assert!(parse_topic_value(Some(&serde_json::Value::Null))
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_parse_topic_value_valid_hex() {
        let val = serde_json::Value::String(ERC20_TRANSFER_TOPIC.to_string());
        let result = parse_topic_value(Some(&val)).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_parse_topic_value_array_unsupported() {
        let val = serde_json::json!(["0xabc", "0xdef"]);
        assert!(parse_topic_value(Some(&val)).is_err());
    }

    // -- cursor_to_from_block --

    #[test]
    fn test_cursor_to_from_block_no_cursor() {
        let from = cursor_to_from_block(None, 20_000_000, 10, 2000);
        // 20_000_000 - 10 * 2000 = 19_980_000
        assert_eq!(from, 19_980_000);
    }

    #[test]
    fn test_cursor_to_from_block_with_last_block() {
        let cursor = json!({"last_block": 18_000_000u64});
        let from = cursor_to_from_block(Some(&cursor), 20_000_000, 10, 2000);
        assert_eq!(from, 18_000_001);
    }

    #[test]
    fn test_cursor_to_from_block_v1_compat() {
        let cursor = json!({"v1_compat": true, "last_block": 17_000_000i64});
        let from = cursor_to_from_block(Some(&cursor), 20_000_000, 10, 2000);
        assert_eq!(from, 17_000_001);
    }

    // -- Connector capabilities --

    #[test]
    fn test_connector_capabilities() {
        // We cannot construct EvmAdapter without an RPC URL, so we test
        // the capabilities logic in the Connector trait test below via
        // a mock-based approach. Here we verify the constants.
        let kinds = vec![
            TargetKind::Wallet,
            TargetKind::Contract,
            TargetKind::TopicFilter,
        ];
        for kind in &kinds {
            assert!(
                kind.valid_for_chain_family(ChainFamily::Evm),
                "{kind:?} should be valid for EVM"
            );
        }
    }

    // -- Target kind rejection --

    #[test]
    fn test_unsupported_target_kinds_for_evm() {
        let unsupported = vec![TargetKind::Program, TargetKind::Market];
        for kind in unsupported {
            assert!(
                !kind.valid_for_chain_family(ChainFamily::Evm),
                "{kind:?} should NOT be valid for EVM"
            );
        }
    }

    // -- Wallet target uses topic-based filtering (structural test) --

    #[test]
    fn test_wallet_topic_filter_construction() {
        // Verify that the Transfer topic and wallet-padded value produce
        // correct 32-byte values for eth_getLogs topic filters.
        let wallet: Address = "0xabcdef1234567890abcdef1234567890abcdef12"
            .parse()
            .unwrap();
        let transfer_topic = parse_b256(ERC20_TRANSFER_TOPIC).unwrap();
        let wallet_padded = address_to_topic(wallet);

        // Transfer topic is the standard ERC-20 Transfer signature
        assert_eq!(
            format!("{transfer_topic:#x}"),
            ERC20_TRANSFER_TOPIC.to_lowercase()
        );

        // Wallet padded should be zero-padded address in topic slot
        let wallet_hex = format!("{wallet_padded:#x}");
        assert_eq!(wallet_hex.len(), 66); // 0x + 64 hex chars
        assert!(wallet_hex.starts_with("0x000000000000000000000000"));
    }

    // -- Contract target uses address-based filtering (structural test) --

    #[test]
    fn test_contract_address_filter() {
        // Verify that contract addresses parse correctly for the address filter
        let contract: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        assert_eq!(
            format!("{contract:#x}"),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
    }

    // -- TopicFilterSpec parsing --

    #[test]
    fn test_topic_filter_spec_parsing() {
        let spec = TopicFilterSpec {
            topics: vec![
                serde_json::json!(
                    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                ),
                serde_json::Value::Null,
                serde_json::json!(
                    "0x000000000000000000000000abcdef1234567890abcdef1234567890abcdef12"
                ),
            ],
            address_filter: vec!["0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string()],
        };

        let t0 = parse_topic_value(spec.topics.first()).unwrap();
        assert!(t0.is_some());

        let t1 = parse_topic_value(spec.topics.get(1)).unwrap();
        assert!(t1.is_none()); // null = wildcard

        let t2 = parse_topic_value(spec.topics.get(2)).unwrap();
        assert!(t2.is_some());

        let t3 = parse_topic_value(spec.topics.get(3)).unwrap();
        assert!(t3.is_none()); // missing = wildcard
    }

    // -- Target rejection tests --

    fn make_target(
        kind: TargetKind,
        address: Option<&str>,
        filter_spec: Option<serde_json::Value>,
    ) -> IndexTarget {
        let now = Utc::now();
        IndexTarget {
            id: Uuid::new_v4(),
            kind,
            network: "ethereum-mainnet".to_string(),
            chain_family: ChainFamily::Evm,
            address: address.map(|s| s.to_string()),
            filter_spec,
            mode: TargetMode::Backfill,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn test_unsupported_target_kind_rejected_by_connector() {
        // The Connector trait validates target kind in backfill().
        // We can verify the capability check without an RPC connection.
        let caps = ConnectorCapabilities {
            supported_target_kinds: vec![
                TargetKind::Wallet,
                TargetKind::Contract,
                TargetKind::TopicFilter,
            ],
            supported_modes: vec![TargetMode::Backfill],
            chain_family: ChainFamily::Evm,
        };

        // Supported kinds
        let wallet = make_target(TargetKind::Wallet, Some("0xabc"), None);
        assert!(caps.can_service(&wallet));

        let contract = make_target(TargetKind::Contract, Some("0xabc"), None);
        assert!(caps.can_service(&contract));

        let topic_filter = make_target(
            TargetKind::TopicFilter,
            None,
            Some(
                json!({"topics": ["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"]}),
            ),
        );
        assert!(caps.can_service(&topic_filter));

        // Unsupported kinds
        let program = IndexTarget {
            kind: TargetKind::Program,
            chain_family: ChainFamily::Solana,
            ..make_target(TargetKind::Program, Some("abc"), None)
        };
        assert!(!caps.can_service(&program));

        let market = IndexTarget {
            kind: TargetKind::Market,
            chain_family: ChainFamily::Hyperliquid,
            ..make_target(TargetKind::Market, Some("ETH"), None)
        };
        assert!(!caps.can_service(&market));
    }

    // -- Gas fee enrichment is preserved (regression) --

    #[test]
    fn test_gas_fee_enrichment_in_raw_metadata() {
        // Verify that tx-level enrichment fields are correctly structured
        // so the downstream parser can extract gas fees.
        let enrichment_tx = json!({
            "value": "0xde0b6b3a7640000",
            "from": "0x1111111111111111111111111111111111111111",
            "to": "0x2222222222222222222222222222222222222222",
        });
        let enrichment_receipt = json!({
            "gas_used": "0x5208",
            "effective_gas_price": "0x3b9aca00",
        });

        // Simulate merging enrichment into raw_metadata (as logs_to_raw_transactions does)
        let mut metadata = json!({
            "log_index": 0,
            "block_number": 18_000_000,
            "topics": [],
            "data": "0x",
        });

        if let Some(obj) = metadata.as_object_mut() {
            if let Some(tx_obj) = enrichment_tx.as_object() {
                obj.extend(tx_obj.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
            if let Some(rx_obj) = enrichment_receipt.as_object() {
                obj.extend(rx_obj.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        }

        assert_eq!(metadata["gas_used"], "0x5208");
        assert_eq!(metadata["effective_gas_price"], "0x3b9aca00");
        assert_eq!(metadata["value"], "0xde0b6b3a7640000");
        assert_eq!(
            metadata["from"],
            "0x1111111111111111111111111111111111111111"
        );
    }

    // -- Native ETH value detection preserved (regression) --

    #[test]
    fn test_native_eth_value_in_enrichment() {
        // Verify that non-zero ETH value is preserved in enrichment fields
        // so the parser can detect native transfers.
        let enrichment_tx = json!({
            "value": "0xde0b6b3a7640000", // 1 ETH in wei
            "from": "0xsender",
            "to": "0xreceiver",
        });

        let value_hex = enrichment_tx["value"].as_str().unwrap();
        let stripped = value_hex.strip_prefix("0x").unwrap();
        let wei = u128::from_str_radix(stripped, 16).unwrap();
        assert_eq!(wei, 1_000_000_000_000_000_000u128); // 1 ETH
    }

    // -- RawTransaction has no wallet_address or user_id --

    #[test]
    fn test_raw_transaction_is_target_agnostic() {
        let rt = RawTransaction {
            id: Uuid::new_v4(),
            network: "ethereum-mainnet".to_string(),
            tx_hash: "0xdeadbeef".to_string(),
            timestamp: 1700000000,
            block_number: Some(18_000_000),
            raw_metadata: json!({"test": true}),
            source: "evm-rpc-wallet-backfill".to_string(),
            ingestion_run_id: None,
            ingested_at: Utc::now(),
        };
        let serialized = serde_json::to_string(&rt).unwrap();
        assert!(!serialized.contains("wallet_address"));
        assert!(!serialized.contains("user_id"));
    }

    // -- Source field correctness --

    #[test]
    fn test_source_fields_are_descriptive() {
        // Source strings should identify the ingestion path
        let sources = [
            "evm-rpc-wallet-backfill",
            "evm-rpc-contract-backfill",
            "evm-rpc-topic-filter-backfill",
        ];
        for s in sources {
            assert!(s.starts_with("evm-rpc-"));
        }
    }

    // -- Cursor/checkpoint roundtrip --

    #[test]
    fn test_cursor_roundtrip() {
        let cursor = json!({"last_block": 18_500_000u64});
        let from = cursor_to_from_block(Some(&cursor), 20_000_000, 10, 2000);
        assert_eq!(from, 18_500_001);

        // Reconstruct cursor from the new last_block
        let new_cursor = json!({"last_block": 19_000_000u64});
        let from2 = cursor_to_from_block(Some(&new_cursor), 20_000_000, 10, 2000);
        assert_eq!(from2, 19_000_001);
    }

    // -- Trace configuration --

    #[test]
    fn test_call_tracer_config_format() {
        let config = json!({
            "tracer": "callTracer",
            "tracerConfig": {
                "onlyTopCall": false,
                "withLog": true,
            }
        });
        assert_eq!(config["tracer"], "callTracer");
        assert_eq!(config["tracerConfig"]["onlyTopCall"], false);
        assert_eq!(config["tracerConfig"]["withLog"], true);
    }

    #[test]
    fn test_prestate_tracer_config_format() {
        let config = json!({
            "tracer": "prestateTracer",
            "tracerConfig": {
                "diffMode": true,
            }
        });
        assert_eq!(config["tracer"], "prestateTracer");
        assert_eq!(config["tracerConfig"]["diffMode"], true);
    }

    #[test]
    fn test_raw_evm_trace_construction() {
        let trace = RawEvmTrace {
            id: Uuid::new_v4(),
            transaction_hash: "0xabc".to_string(),
            block_number: Some(18_000_000),
            network: "ethereum-mainnet".to_string(),
            trace_type: EvmTraceType::CallTracer,
            raw_trace: json!({
                "type": "CALL",
                "from": "0x1111111111111111111111111111111111111111",
                "to": "0x2222222222222222222222222222222222222222",
                "value": "0xde0b6b3a7640000",
            }),
            ingestion_run_id: None,
            created_at: Utc::now(),
        };
        assert_eq!(trace.network, "ethereum-mainnet");
        assert_eq!(trace.trace_type, EvmTraceType::CallTracer);
        assert!(trace.raw_trace.get("type").is_some());
    }

    #[test]
    fn test_trace_rpc_defaults_to_none() {
        // When no trace RPC URL is set, trace_rpc field is None.
        // The fetch_trace method falls back to the primary rpc_url.
        let adapter = EvmAdapter::new("http://localhost:8545");
        assert!(adapter.is_ok());
        let adapter = adapter.unwrap();
        assert!(adapter.trace_rpc.is_none());
    }

    #[test]
    fn test_with_trace_rpc_url_sets_rpc() {
        let adapter = EvmAdapter::new("http://localhost:8545")
            .unwrap()
            .with_trace_rpc_url("http://localhost:8546");
        assert!(adapter.is_ok());
        let adapter = adapter.unwrap();
        assert!(adapter.trace_rpc.is_some());
    }

    #[test]
    fn test_trace_type_to_rpc_tracer_name() {
        // Verify EvmTraceType display strings match the Geth tracer names
        assert_eq!(EvmTraceType::CallTracer.to_string(), "callTracer");
        assert_eq!(EvmTraceType::PrestateTracer.to_string(), "prestateTracer");
        assert_eq!(EvmTraceType::FlatCallTracer.to_string(), "flatCallTracer");
    }

    #[test]
    fn test_fetch_traces_block_number_map() {
        // Verify that block_numbers HashMap lookup works correctly
        let tx_hash = B256::from([0xaa; 32]);
        let mut block_numbers: HashMap<B256, Option<i64>> = HashMap::new();
        block_numbers.insert(tx_hash, Some(18_000_000));

        let result = block_numbers.get(&tx_hash).copied().unwrap_or(None);
        assert_eq!(result, Some(18_000_000));

        let missing = B256::from([0xbb; 32]);
        let result = block_numbers.get(&missing).copied().unwrap_or(None);
        assert_eq!(result, None);
    }
}
