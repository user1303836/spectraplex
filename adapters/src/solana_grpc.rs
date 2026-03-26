use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterTransactions,
};

use spectraplex_core::connector::{Connector, ConnectorCapabilities};
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, Transaction};
use spectraplex_core::provider::{NetworkContext, ProviderCapability};
use spectraplex_core::v2::{
    ChainFamily, IndexTarget, IngestionBatch, RawTransaction, TargetKind, TargetMode,
};

/// Default program IDs to monitor when none are specified.
const DEFAULT_PROGRAM_IDS: &[&str] = &[
    "11111111111111111111111111111111",             // System Program
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",  // Token Program
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", // Associated Token Program
];

/// Configuration for the gRPC streaming adapter.
pub struct GrpcStreamConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
    pub program_ids: Vec<String>,
    pub commitment: CommitmentLevel,
    pub max_retries: u32,
    pub channel_capacity: usize,
}

impl Default for GrpcStreamConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            x_token: None,
            program_ids: DEFAULT_PROGRAM_IDS.iter().map(|s| s.to_string()).collect(),
            commitment: CommitmentLevel::Confirmed,
            max_retries: 10,
            channel_capacity: 10_000,
        }
    }
}

/// Tracks the last processed slot for checkpoint-based reconnection.
#[derive(Clone)]
pub struct SlotCheckpoint {
    last_slot: Arc<AtomicU64>,
}

impl Default for SlotCheckpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl SlotCheckpoint {
    pub fn new() -> Self {
        Self {
            last_slot: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn update(&self, slot: u64) {
        self.last_slot.fetch_max(slot, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.last_slot.load(Ordering::Relaxed)
    }
}

/// The Yellowstone gRPC adapter for real-time Solana transaction streaming.
///
/// Supports two modes:
/// - `fetch_history`: one-shot subscription that collects up to `limit` transactions
/// - `stream_transactions`: long-running streaming via a bounded channel
pub struct SolanaGrpcAdapter {
    config: GrpcStreamConfig,
    checkpoint: SlotCheckpoint,
}

impl SolanaGrpcAdapter {
    pub fn new(endpoint: &str, x_token: Option<String>) -> Self {
        Self {
            config: GrpcStreamConfig {
                endpoint: endpoint.to_string(),
                x_token,
                ..Default::default()
            },
            checkpoint: SlotCheckpoint::new(),
        }
    }

    pub fn with_config(config: GrpcStreamConfig) -> Self {
        Self {
            config,
            checkpoint: SlotCheckpoint::new(),
        }
    }

    /// Create a new adapter from a `NetworkContext`.
    ///
    /// Resolves the `Stream` capability provider to obtain the gRPC
    /// endpoint URL and optional auth token. Returns an error if no
    /// suitable provider is available.
    pub fn from_network_context(ctx: &NetworkContext) -> anyhow::Result<Self> {
        let provider = ctx.provider(ProviderCapability::Stream).ok_or_else(|| {
            anyhow::anyhow!(
                "no gRPC provider with 'stream' capability for network '{}'",
                ctx.network
            )
        })?;

        Ok(Self {
            config: GrpcStreamConfig {
                endpoint: provider.url.clone(),
                x_token: provider.token.clone(),
                ..Default::default()
            },
            checkpoint: SlotCheckpoint::new(),
        })
    }

    pub fn set_program_ids(&mut self, program_ids: Vec<String>) {
        self.config.program_ids = program_ids;
    }

    pub fn checkpoint(&self) -> &SlotCheckpoint {
        &self.checkpoint
    }

    /// Build the gRPC subscription request filtered by program IDs.
    fn build_subscribe_request(&self, from_slot: Option<u64>) -> SubscribeRequest {
        let mut transactions = HashMap::new();
        transactions.insert(
            "tx_filter".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include: self.config.program_ids.clone(),
                account_exclude: vec![],
                account_required: vec![],
                signature: None,
            },
        );

        SubscribeRequest {
            accounts: HashMap::new(),
            slots: HashMap::new(),
            transactions,
            transactions_status: HashMap::new(),
            blocks: HashMap::new(),
            blocks_meta: HashMap::new(),
            entry: HashMap::new(),
            commitment: Some(self.config.commitment as i32),
            accounts_data_slice: vec![],
            ping: None,
            from_slot,
        }
    }

    async fn connect_client(
        &self,
    ) -> anyhow::Result<GeyserGrpcClient<impl yellowstone_grpc_client::Interceptor + Clone>> {
        let builder = GeyserGrpcClient::build_from_shared(self.config.endpoint.clone())?
            .x_token(self.config.x_token.clone())?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(10))
            .max_decoding_message_size(64 * 1024 * 1024);

        let client = builder.connect().await?;
        Ok(client)
    }

    /// Start a long-running streaming task. Returns a receiver for transactions
    /// and a JoinHandle for the background task.
    ///
    /// The stream will automatically reconnect on failure with exponential backoff,
    /// resuming from the last checkpointed slot.
    pub fn stream_transactions(
        &self,
    ) -> (mpsc::Receiver<Transaction>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(self.config.channel_capacity);
        let config_endpoint = self.config.endpoint.clone();
        let config_x_token = self.config.x_token.clone();
        let config_program_ids = self.config.program_ids.clone();
        let config_commitment = self.config.commitment;
        let config_max_retries = self.config.max_retries;
        let checkpoint = self.checkpoint.clone();

        let handle = tokio::spawn(async move {
            let adapter = SolanaGrpcAdapter {
                config: GrpcStreamConfig {
                    endpoint: config_endpoint,
                    x_token: config_x_token,
                    program_ids: config_program_ids,
                    commitment: config_commitment,
                    max_retries: config_max_retries,
                    ..Default::default()
                },
                checkpoint: checkpoint.clone(),
            };

            let mut retry_count: u32 = 0;

            loop {
                let from_slot = {
                    let last = checkpoint.get();
                    if last > 0 {
                        Some(last)
                    } else {
                        None
                    }
                };

                info!(
                    "Connecting to gRPC endpoint: {} (from_slot: {:?}, attempt: {})",
                    adapter.config.endpoint,
                    from_slot,
                    retry_count + 1,
                );

                match adapter.run_stream(&tx, from_slot).await {
                    Ok(()) => {
                        info!("gRPC stream ended normally");
                        break;
                    }
                    Err(e) => {
                        error!("gRPC stream error: {}", e);
                        retry_count += 1;
                        if retry_count > config_max_retries {
                            error!(
                                "Exceeded max retries ({}). Stopping gRPC stream.",
                                config_max_retries
                            );
                            break;
                        }
                        let backoff = Duration::from_secs(2u64.saturating_pow(retry_count.min(6)));
                        warn!(
                            "Reconnecting in {:?} (retry {}/{})",
                            backoff, retry_count, config_max_retries
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        });

        (rx, handle)
    }

    async fn run_stream(
        &self,
        tx: &mpsc::Sender<Transaction>,
        from_slot: Option<u64>,
    ) -> anyhow::Result<()> {
        let mut client = self.connect_client().await?;
        let request = self.build_subscribe_request(from_slot);
        let mut stream = client.subscribe_once(request).await?;

        info!("gRPC subscription established");

        while let Some(msg) = stream.next().await {
            let update = msg?;

            match update.update_oneof {
                Some(UpdateOneof::Transaction(tx_update)) => {
                    let slot = tx_update.slot;
                    self.checkpoint.update(slot);

                    if let Some(tx_info) = tx_update.transaction {
                        match convert_grpc_transaction(&tx_info, slot) {
                            Ok(transaction) => {
                                if tx.send(transaction).await.is_err() {
                                    info!("Channel receiver dropped, stopping stream");
                                    return Ok(());
                                }
                            }
                            Err(e) => {
                                warn!("Failed to convert gRPC transaction: {}", e);
                            }
                        }
                    }
                }
                Some(UpdateOneof::Ping(_)) => {
                    // Pings are keepalives, no action needed
                }
                _ => {
                    // Ignore other update types (slots, blocks, etc.)
                }
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl ChainIngestor for SolanaGrpcAdapter {
    /// One-shot fetch: subscribes to the gRPC stream and collects up to `limit` transactions.
    ///
    /// The `wallet` parameter is used as the wallet_address on collected transactions.
    /// For real-time streaming, use `stream_transactions()` instead.
    async fn fetch_history(
        &self,
        wallet: &str,
        limit: usize,
        user_id: Uuid,
        _checkpoint: Option<&IndexerCheckpoint>,
    ) -> anyhow::Result<Vec<Transaction>> {
        let mut client = self.connect_client().await?;

        let from_slot = {
            let last = self.checkpoint.get();
            if last > 0 {
                Some(last)
            } else {
                None
            }
        };
        let request = self.build_subscribe_request(from_slot);
        let mut stream = client.subscribe_once(request).await?;

        info!(
            "gRPC fetch_history: subscribing for up to {} transactions (wallet: {})",
            limit, wallet
        );

        let mut transactions = Vec::new();

        while let Some(msg) = stream.next().await {
            let update = msg?;

            if let Some(UpdateOneof::Transaction(tx_update)) = update.update_oneof {
                let slot = tx_update.slot;
                self.checkpoint.update(slot);

                if let Some(tx_info) = tx_update.transaction {
                    match convert_grpc_transaction(&tx_info, slot) {
                        Ok(mut transaction) => {
                            transaction.wallet_address = wallet.to_string();
                            transaction.user_id = user_id;
                            transactions.push(transaction);

                            if transactions.len() >= limit {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!("Failed to convert gRPC transaction: {}", e);
                        }
                    }
                }
            }
        }

        info!(
            "gRPC fetch_history: collected {} transactions",
            transactions.len()
        );
        Ok(transactions)
    }
}

// ---------------------------------------------------------------------------
// V2 Connector implementation
// ---------------------------------------------------------------------------

/// Build a target-kind-aware gRPC subscription request.
///
/// - Wallet target: `account_include = [wallet_pubkey]`
/// - Program target: `account_include = [program_id]`
/// - Account target: `account_include = [account_pubkey]`
///
/// The `account_include` field on Yellowstone gRPC's transaction filter matches
/// any transaction that references one of the listed accounts in its account
/// keys. This is the correct semantic for all three target kinds: it captures
/// every transaction that touches the specified address.
fn build_target_subscribe_request(
    target: &IndexTarget,
    commitment: CommitmentLevel,
    from_slot: Option<u64>,
) -> anyhow::Result<SubscribeRequest> {
    let address = target
        .address
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("{:?} target must have an address", target.kind))?;

    let mut transactions = HashMap::new();
    transactions.insert(
        "tx_filter".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: vec![address.to_string()],
            account_exclude: vec![],
            account_required: vec![],
            signature: None,
        },
    );

    Ok(SubscribeRequest {
        accounts: HashMap::new(),
        slots: HashMap::new(),
        transactions,
        transactions_status: HashMap::new(),
        blocks: HashMap::new(),
        blocks_meta: HashMap::new(),
        entry: HashMap::new(),
        commitment: Some(commitment as i32),
        accounts_data_slice: vec![],
        ping: None,
        from_slot,
    })
}

/// Convert a Yellowstone gRPC transaction update into a target-agnostic
/// V2 `RawTransaction`. No `wallet_address` or `user_id` fields.
pub fn convert_grpc_to_raw_transaction(
    tx_info: &yellowstone_grpc_proto::prelude::SubscribeUpdateTransactionInfo,
    slot: u64,
    network: &str,
    source: &str,
) -> anyhow::Result<RawTransaction> {
    let tx_hash = bs58::encode(&tx_info.signature).into_string();
    let raw_metadata = build_raw_metadata(tx_info, slot)?;
    let timestamp = extract_block_time(&raw_metadata);

    Ok(RawTransaction {
        id: Uuid::new_v4(),
        network: network.to_string(),
        tx_hash,
        timestamp,
        block_number: Some(slot as i64),
        raw_metadata,
        source: source.to_string(),
        ingestion_run_id: None,
        ingested_at: Utc::now(),
    })
}

/// Extract a from_slot value from an opaque V2 cursor JSON.
///
/// Cursor shape: `{"last_slot": u64, "last_signature": "..."}`.
fn cursor_to_from_slot(cursor: Option<&serde_json::Value>) -> Option<u64> {
    cursor
        .and_then(|c| c.get("last_slot"))
        .and_then(|v| v.as_u64())
}

/// Source string for V2 gRPC ingestion paths.
fn v2_source(target_kind: TargetKind, mode: &str) -> String {
    let kind_str = match target_kind {
        TargetKind::Wallet => "wallet",
        TargetKind::Program => "program",
        TargetKind::Account => "account",
        _ => "unknown",
    };
    format!("solana-grpc-{kind_str}-{mode}")
}

#[async_trait::async_trait]
impl Connector for SolanaGrpcAdapter {
    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supported_target_kinds: vec![
                TargetKind::Wallet,
                TargetKind::Program,
                TargetKind::Account,
            ],
            supported_modes: vec![TargetMode::Backfill, TargetMode::Stream, TargetMode::Both],
            chain_family: ChainFamily::Solana,
        }
    }

    async fn backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        if target.chain_family != ChainFamily::Solana {
            anyhow::bail!(
                "SolanaGrpcAdapter only supports Solana chain family, got {:?}",
                target.chain_family
            );
        }

        match target.kind {
            TargetKind::Wallet | TargetKind::Program | TargetKind::Account => {}
            other => anyhow::bail!(
                "SolanaGrpcAdapter does not support target kind {:?}; supported: wallet, program, account",
                other
            ),
        }

        let from_slot = cursor_to_from_slot(cursor);
        let request = build_target_subscribe_request(target, self.config.commitment, from_slot)?;

        let mut client = self.connect_client().await?;
        let mut stream = client.subscribe_once(request).await?;

        let source = v2_source(target.kind, "backfill");
        let network = &target.network;
        let mut records = Vec::new();

        info!(
            "gRPC V2 backfill: subscribing for up to {} transactions (target: {:?}, address: {:?})",
            limit, target.kind, target.address
        );

        while let Some(msg) = stream.next().await {
            let update = msg?;

            if let Some(UpdateOneof::Transaction(tx_update)) = update.update_oneof {
                let slot = tx_update.slot;
                self.checkpoint.update(slot);

                if let Some(tx_info) = tx_update.transaction {
                    match convert_grpc_to_raw_transaction(&tx_info, slot, network, &source) {
                        Ok(raw_tx) => {
                            records.push(raw_tx);
                            if records.len() >= limit {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to convert gRPC transaction to RawTransaction: {}",
                                e
                            );
                        }
                    }
                }
            }
        }

        info!("gRPC V2 backfill: collected {} records", records.len());

        Ok(IngestionBatch {
            records,
            checkpoint: None,
            run_metadata: None,
        })
    }

    async fn stream(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
    ) -> anyhow::Result<mpsc::Receiver<RawTransaction>> {
        if target.chain_family != ChainFamily::Solana {
            anyhow::bail!(
                "SolanaGrpcAdapter only supports Solana chain family, got {:?}",
                target.chain_family
            );
        }

        match target.kind {
            TargetKind::Wallet | TargetKind::Program | TargetKind::Account => {}
            other => anyhow::bail!(
                "SolanaGrpcAdapter does not support streaming for target kind {:?}",
                other
            ),
        }

        let from_slot = cursor_to_from_slot(cursor);
        let (tx, rx) = mpsc::channel(self.config.channel_capacity);

        let config_endpoint = self.config.endpoint.clone();
        let config_x_token = self.config.x_token.clone();
        let config_commitment = self.config.commitment;
        let config_max_retries = self.config.max_retries;
        let target_kind = target.kind;
        let target_address = target
            .address
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{:?} target must have an address", target.kind))?;
        let target_network = target.network.clone();
        let checkpoint = self.checkpoint.clone();

        let source = v2_source(target.kind, "stream");

        tokio::spawn(async move {
            let mut retry_count: u32 = 0;

            loop {
                let current_slot = {
                    let last = checkpoint.get();
                    if last > 0 {
                        Some(last)
                    } else {
                        from_slot
                    }
                };

                info!(
                    "V2 gRPC stream connecting: endpoint={}, target_kind={:?}, address={}, from_slot={:?}, attempt={}",
                    config_endpoint, target_kind, target_address, current_slot, retry_count + 1,
                );

                let adapter = SolanaGrpcAdapter {
                    config: GrpcStreamConfig {
                        endpoint: config_endpoint.clone(),
                        x_token: config_x_token.clone(),
                        commitment: config_commitment,
                        max_retries: config_max_retries,
                        ..Default::default()
                    },
                    checkpoint: checkpoint.clone(),
                };

                let target_for_request = IndexTarget {
                    id: Uuid::nil(),
                    kind: target_kind,
                    network: target_network.clone(),
                    chain_family: ChainFamily::Solana,
                    address: Some(target_address.clone()),
                    filter_spec: None,
                    mode: TargetMode::Stream,
                    label: None,
                    owner_id: None,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                };

                match adapter
                    .run_v2_stream(&tx, &target_for_request, &source, current_slot)
                    .await
                {
                    Ok(()) => {
                        info!("V2 gRPC stream ended normally");
                        break;
                    }
                    Err(e) => {
                        error!("V2 gRPC stream error: {}", e);
                        retry_count += 1;
                        if retry_count > config_max_retries {
                            error!(
                                "Exceeded max retries ({}). Stopping V2 gRPC stream.",
                                config_max_retries
                            );
                            break;
                        }
                        let backoff = Duration::from_secs(2u64.saturating_pow(retry_count.min(6)));
                        warn!(
                            "V2 stream reconnecting in {:?} (retry {}/{})",
                            backoff, retry_count, config_max_retries
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        });

        Ok(rx)
    }
}

impl SolanaGrpcAdapter {
    /// Internal stream runner for V2 Connector, emitting `RawTransaction`s.
    async fn run_v2_stream(
        &self,
        tx: &mpsc::Sender<RawTransaction>,
        target: &IndexTarget,
        source: &str,
        from_slot: Option<u64>,
    ) -> anyhow::Result<()> {
        let request = build_target_subscribe_request(target, self.config.commitment, from_slot)?;
        let mut client = self.connect_client().await?;
        let mut stream = client.subscribe_once(request).await?;

        info!("V2 gRPC subscription established");

        while let Some(msg) = stream.next().await {
            let update = msg?;

            match update.update_oneof {
                Some(UpdateOneof::Transaction(tx_update)) => {
                    let slot = tx_update.slot;
                    self.checkpoint.update(slot);

                    if let Some(tx_info) = tx_update.transaction {
                        match convert_grpc_to_raw_transaction(
                            &tx_info,
                            slot,
                            &target.network,
                            source,
                        ) {
                            Ok(raw_tx) => {
                                if tx.send(raw_tx).await.is_err() {
                                    info!("V2 stream receiver dropped, stopping");
                                    return Ok(());
                                }
                            }
                            Err(e) => {
                                warn!("Failed to convert gRPC transaction: {}", e);
                            }
                        }
                    }
                }
                Some(UpdateOneof::Ping(_)) => {}
                _ => {}
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// V1 Legacy conversion (preserved for backward compatibility)
// ---------------------------------------------------------------------------

/// Convert a Yellowstone gRPC transaction update into a Bronze-layer Transaction.
///
/// Extracts the signature as tx_hash (base58-encoded), the block time from
/// the transaction metadata, and stores the full protobuf data as JSON
/// in raw_metadata.
pub fn convert_grpc_transaction(
    tx_info: &yellowstone_grpc_proto::prelude::SubscribeUpdateTransactionInfo,
    slot: u64,
) -> anyhow::Result<Transaction> {
    let tx_hash = bs58::encode(&tx_info.signature).into_string();
    let raw_metadata = build_raw_metadata(tx_info, slot)?;

    Ok(Transaction {
        id: Uuid::new_v4(),
        user_id: Uuid::nil(),
        wallet_address: String::new(),
        timestamp: extract_block_time(&raw_metadata),
        tx_hash,
        chain: Chain::Solana,
        raw_metadata,
    })
}

fn extract_block_time(raw_metadata: &serde_json::Value) -> i64 {
    raw_metadata
        .get("block_time")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

/// Build a JSON representation of the gRPC transaction data.
/// This mirrors the structure that the RPC adapter produces, making it
/// compatible with the existing parser pipeline.
fn build_raw_metadata(
    tx_info: &yellowstone_grpc_proto::prelude::SubscribeUpdateTransactionInfo,
    slot: u64,
) -> anyhow::Result<serde_json::Value> {
    let signature = bs58::encode(&tx_info.signature).into_string();

    let account_keys: Vec<String> = tx_info
        .transaction
        .as_ref()
        .and_then(|tx| tx.message.as_ref())
        .map(|msg| {
            msg.account_keys
                .iter()
                .map(|k| bs58::encode(k).into_string())
                .collect()
        })
        .unwrap_or_default();

    let (pre_balances, post_balances, fee, pre_token_balances, post_token_balances, log_messages) =
        if let Some(meta) = &tx_info.meta {
            let pre_tok: Vec<serde_json::Value> = meta
                .pre_token_balances
                .iter()
                .map(|tb| {
                    let ui_amount = tb.ui_token_amount.as_ref();
                    json!({
                        "accountIndex": tb.account_index,
                        "mint": tb.mint,
                        "owner": tb.owner,
                        "programId": tb.program_id,
                        "uiTokenAmount": {
                            "uiAmount": ui_amount.map(|a| a.ui_amount).unwrap_or(0.0),
                            "decimals": ui_amount.map(|a| a.decimals).unwrap_or(0),
                            "amount": ui_amount.map(|a| a.amount.as_str()).unwrap_or("0"),
                            "uiAmountString": ui_amount.map(|a| a.ui_amount_string.as_str()).unwrap_or("0"),
                        }
                    })
                })
                .collect();

            let post_tok: Vec<serde_json::Value> = meta
                .post_token_balances
                .iter()
                .map(|tb| {
                    let ui_amount = tb.ui_token_amount.as_ref();
                    json!({
                        "accountIndex": tb.account_index,
                        "mint": tb.mint,
                        "owner": tb.owner,
                        "programId": tb.program_id,
                        "uiTokenAmount": {
                            "uiAmount": ui_amount.map(|a| a.ui_amount).unwrap_or(0.0),
                            "decimals": ui_amount.map(|a| a.decimals).unwrap_or(0),
                            "amount": ui_amount.map(|a| a.amount.as_str()).unwrap_or("0"),
                            "uiAmountString": ui_amount.map(|a| a.ui_amount_string.as_str()).unwrap_or("0"),
                        }
                    })
                })
                .collect();

            (
                meta.pre_balances.clone(),
                meta.post_balances.clone(),
                meta.fee,
                pre_tok,
                post_tok,
                meta.log_messages.clone(),
            )
        } else {
            (vec![], vec![], 0, vec![], vec![], vec![])
        };

    let err_value = tx_info
        .meta
        .as_ref()
        .and_then(|m| m.err.as_ref())
        .map(|_| json!("TransactionError"))
        .unwrap_or(serde_json::Value::Null);

    let status = if err_value.is_null() {
        json!({"Ok": null})
    } else {
        json!({"Err": "error"})
    };

    Ok(json!({
        "slot": slot,
        "transaction": {
            "signatures": [signature],
            "message": {
                "accountKeys": account_keys.iter().map(|k| {
                    json!({"pubkey": k, "signer": false, "writable": false})
                }).collect::<Vec<_>>(),
                "instructions": [],
                "recentBlockhash": "11111111111111111111111111111111"
            }
        },
        "meta": {
            "err": err_value,
            "fee": fee,
            "preBalances": pre_balances,
            "postBalances": post_balances,
            "innerInstructions": [],
            "logMessages": log_messages,
            "preTokenBalances": pre_token_balances,
            "postTokenBalances": post_token_balances,
            "rewards": [],
            "status": status
        },
        // Yellowstone gRPC transaction updates don't include block_time;
        // it's a block-level attribute. Use current time as approximation
        // for real-time streaming. For historical accuracy, callers should
        // enrich timestamps from block data.
        "block_time": chrono::Utc::now().timestamp()
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use yellowstone_grpc_proto::prelude::{
        SubscribeUpdateTransactionInfo, TokenBalance, TransactionStatusMeta, UiTokenAmount,
    };

    fn make_test_signature() -> Vec<u8> {
        vec![1u8; 64]
    }

    fn make_test_tx_info(
        pre_balances: Vec<u64>,
        post_balances: Vec<u64>,
        account_keys: Vec<Vec<u8>>,
    ) -> SubscribeUpdateTransactionInfo {
        use yellowstone_grpc_proto::prelude::{Message, MessageHeader, Transaction as ProtoTx};

        SubscribeUpdateTransactionInfo {
            signature: make_test_signature(),
            is_vote: false,
            transaction: Some(ProtoTx {
                signatures: vec![make_test_signature()],
                message: Some(Message {
                    header: Some(MessageHeader {
                        num_required_signatures: 1,
                        num_readonly_signed_accounts: 0,
                        num_readonly_unsigned_accounts: 0,
                    }),
                    account_keys,
                    recent_blockhash: vec![0u8; 32],
                    instructions: vec![],
                    versioned: false,
                    address_table_lookups: vec![],
                }),
            }),
            meta: Some(TransactionStatusMeta {
                err: None,
                fee: 5000,
                pre_balances,
                post_balances,
                inner_instructions: vec![],
                inner_instructions_none: false,
                log_messages: vec!["Program log: test".to_string()],
                log_messages_none: false,
                pre_token_balances: vec![],
                post_token_balances: vec![],
                rewards: vec![],
                loaded_writable_addresses: vec![],
                loaded_readonly_addresses: vec![],
                return_data: None,
                return_data_none: true,
                compute_units_consumed: Some(1000),
                cost_units: None,
            }),
            index: 0,
        }
    }

    #[test]
    fn test_convert_grpc_transaction_basic() {
        let account_keys = vec![vec![1u8; 32], vec![2u8; 32]];
        let tx_info = make_test_tx_info(
            vec![10_000_000_000, 0],
            vec![9_500_000_000, 500_000_000],
            account_keys,
        );

        let result = convert_grpc_transaction(&tx_info, 12345);
        assert!(result.is_ok());

        let transaction = result.unwrap();
        assert!(!transaction.tx_hash.is_empty());
        assert!(matches!(transaction.chain, Chain::Solana));
        assert_eq!(transaction.wallet_address, "");

        let meta = &transaction.raw_metadata;
        assert_eq!(meta["slot"], 12345);
        assert_eq!(meta["meta"]["fee"], 5000);
        assert_eq!(meta["meta"]["preBalances"][0], 10_000_000_000u64);
        assert_eq!(meta["meta"]["postBalances"][0], 9_500_000_000u64);
    }

    #[test]
    fn test_convert_grpc_transaction_signature_encoding() {
        let tx_info = make_test_tx_info(vec![0], vec![0], vec![vec![0u8; 32]]);
        let result = convert_grpc_transaction(&tx_info, 100);
        let transaction = result.unwrap();

        let decoded = bs58::decode(&transaction.tx_hash).into_vec().unwrap();
        assert_eq!(decoded.len(), 64);
        assert!(decoded.iter().all(|&b| b == 1));
    }

    #[test]
    fn test_convert_grpc_transaction_with_token_balances() {
        let mut tx_info =
            make_test_tx_info(vec![1_000_000_000], vec![999_000_000], vec![vec![1u8; 32]]);

        if let Some(meta) = tx_info.meta.as_mut() {
            meta.pre_token_balances = vec![TokenBalance {
                account_index: 1,
                mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
                ui_token_amount: Some(UiTokenAmount {
                    ui_amount: 100.0,
                    decimals: 6,
                    amount: "100000000".to_string(),
                    ui_amount_string: "100.0".to_string(),
                }),
                owner: "WalletOwner11111111111111111111111111111111".to_string(),
                program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            }];
            meta.post_token_balances = vec![TokenBalance {
                account_index: 1,
                mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
                ui_token_amount: Some(UiTokenAmount {
                    ui_amount: 50.0,
                    decimals: 6,
                    amount: "50000000".to_string(),
                    ui_amount_string: "50.0".to_string(),
                }),
                owner: "WalletOwner11111111111111111111111111111111".to_string(),
                program_id: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            }];
        }

        let result = convert_grpc_transaction(&tx_info, 500);
        assert!(result.is_ok());

        let transaction = result.unwrap();
        let meta = &transaction.raw_metadata["meta"];

        let pre_tok = &meta["preTokenBalances"];
        assert_eq!(
            pre_tok[0]["mint"],
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        );
        assert_eq!(pre_tok[0]["uiTokenAmount"]["uiAmount"], 100.0);

        let post_tok = &meta["postTokenBalances"];
        assert_eq!(post_tok[0]["uiTokenAmount"]["uiAmount"], 50.0);
    }

    #[test]
    fn test_convert_grpc_transaction_no_meta() {
        let mut tx_info = make_test_tx_info(vec![], vec![], vec![]);
        tx_info.meta = None;

        let result = convert_grpc_transaction(&tx_info, 999);
        assert!(result.is_ok());

        let transaction = result.unwrap();
        assert_eq!(transaction.raw_metadata["meta"]["fee"], 0);
        assert_eq!(transaction.raw_metadata["meta"]["preBalances"], json!([]));
    }

    #[test]
    fn test_convert_grpc_transaction_failed_tx() {
        let mut tx_info = make_test_tx_info(vec![1000], vec![1000], vec![vec![0u8; 32]]);
        if let Some(meta) = tx_info.meta.as_mut() {
            meta.err =
                Some(yellowstone_grpc_proto::prelude::TransactionError { err: vec![1, 2, 3] });
        }

        let result = convert_grpc_transaction(&tx_info, 42);
        assert!(result.is_ok());

        let transaction = result.unwrap();
        assert!(!transaction.raw_metadata["meta"]["err"].is_null());
    }

    #[test]
    fn test_slot_checkpoint() {
        let checkpoint = SlotCheckpoint::new();
        assert_eq!(checkpoint.get(), 0);

        checkpoint.update(100);
        assert_eq!(checkpoint.get(), 100);

        checkpoint.update(50);
        assert_eq!(checkpoint.get(), 100);

        checkpoint.update(200);
        assert_eq!(checkpoint.get(), 200);
    }

    #[test]
    fn test_build_subscribe_request() {
        let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        let request = adapter.build_subscribe_request(None);

        assert!(request.transactions.contains_key("tx_filter"));
        let filter = &request.transactions["tx_filter"];
        assert_eq!(filter.vote, Some(false));
        assert_eq!(filter.failed, Some(false));
        assert_eq!(filter.account_include.len(), 3);
        assert!(request.from_slot.is_none());
    }

    #[test]
    fn test_build_subscribe_request_with_from_slot() {
        let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        let request = adapter.build_subscribe_request(Some(12345));

        assert_eq!(request.from_slot, Some(12345));
        assert_eq!(request.commitment, Some(CommitmentLevel::Confirmed as i32));
    }

    #[test]
    fn test_build_subscribe_request_custom_programs() {
        let mut adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        adapter.set_program_ids(vec![
            "CustomProgram111111111111111111111111111111".to_string()
        ]);

        let request = adapter.build_subscribe_request(None);
        let filter = &request.transactions["tx_filter"];
        assert_eq!(filter.account_include.len(), 1);
        assert_eq!(
            filter.account_include[0],
            "CustomProgram111111111111111111111111111111"
        );
    }

    #[test]
    fn test_default_config() {
        let config = GrpcStreamConfig::default();
        assert_eq!(config.program_ids.len(), 3);
        assert_eq!(config.max_retries, 10);
        assert_eq!(config.channel_capacity, 10_000);
        assert_eq!(config.commitment, CommitmentLevel::Confirmed);
    }

    #[test]
    fn test_adapter_with_config() {
        let config = GrpcStreamConfig {
            endpoint: "http://test:10000".to_string(),
            x_token: Some("test-token".to_string()),
            program_ids: vec!["TestProg1111111111111111111111111111111111".to_string()],
            commitment: CommitmentLevel::Finalized,
            max_retries: 5,
            channel_capacity: 500,
        };

        let adapter = SolanaGrpcAdapter::with_config(config);
        assert_eq!(adapter.config.endpoint, "http://test:10000");
        assert_eq!(adapter.config.x_token, Some("test-token".to_string()));
        assert_eq!(adapter.config.program_ids.len(), 1);
        assert_eq!(adapter.config.max_retries, 5);
        assert_eq!(adapter.config.channel_capacity, 500);
    }

    // -----------------------------------------------------------------------
    // V2 Connector tests
    // -----------------------------------------------------------------------

    fn make_target(kind: TargetKind, address: Option<&str>, network: &str) -> IndexTarget {
        let now = Utc::now();
        IndexTarget {
            id: Uuid::new_v4(),
            kind,
            network: network.to_string(),
            chain_family: ChainFamily::Solana,
            address: address.map(|s| s.to_string()),
            filter_spec: None,
            mode: TargetMode::Both,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    // -- Connector capabilities --

    #[test]
    fn test_v2_connector_capabilities() {
        let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        let caps = adapter.capabilities();

        assert_eq!(caps.chain_family, ChainFamily::Solana);
        assert_eq!(
            caps.supported_target_kinds,
            vec![TargetKind::Wallet, TargetKind::Program, TargetKind::Account]
        );
        assert_eq!(
            caps.supported_modes,
            vec![TargetMode::Backfill, TargetMode::Stream, TargetMode::Both]
        );
    }

    #[test]
    fn test_v2_connector_can_service_wallet() {
        let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        let caps = adapter.capabilities();
        let target = make_target(
            TargetKind::Wallet,
            Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"),
            "solana-mainnet",
        );
        assert!(caps.can_service(&target));
    }

    #[test]
    fn test_v2_connector_can_service_program() {
        let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        let caps = adapter.capabilities();
        let target = make_target(
            TargetKind::Program,
            Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            "solana-mainnet",
        );
        assert!(caps.can_service(&target));
    }

    #[test]
    fn test_v2_connector_can_service_account() {
        let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        let caps = adapter.capabilities();
        let target = make_target(
            TargetKind::Account,
            Some("8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC"),
            "solana-mainnet",
        );
        assert!(caps.can_service(&target));
    }

    #[test]
    fn test_v2_connector_rejects_contract() {
        let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        let caps = adapter.capabilities();
        let mut target = make_target(TargetKind::Contract, Some("0xabc"), "ethereum-mainnet");
        target.chain_family = ChainFamily::Evm;
        assert!(!caps.can_service(&target));
    }

    #[test]
    fn test_v2_connector_rejects_market() {
        let adapter = SolanaGrpcAdapter::new("http://localhost:10000", None);
        let caps = adapter.capabilities();
        let mut target = make_target(TargetKind::Market, Some("ETH"), "hypercore-mainnet");
        target.chain_family = ChainFamily::Hyperliquid;
        assert!(!caps.can_service(&target));
    }

    // -- Target-aware subscription request construction --

    #[test]
    fn test_build_target_subscribe_request_wallet() {
        let target = make_target(
            TargetKind::Wallet,
            Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"),
            "solana-mainnet",
        );
        let request =
            build_target_subscribe_request(&target, CommitmentLevel::Confirmed, None).unwrap();

        let filter = &request.transactions["tx_filter"];
        assert_eq!(filter.account_include.len(), 1);
        assert_eq!(
            filter.account_include[0],
            "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"
        );
        assert_eq!(filter.vote, Some(false));
        assert_eq!(filter.failed, Some(false));
        assert!(filter.account_exclude.is_empty());
        assert!(filter.account_required.is_empty());
        assert!(request.from_slot.is_none());
    }

    #[test]
    fn test_build_target_subscribe_request_program() {
        let target = make_target(
            TargetKind::Program,
            Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            "solana-mainnet",
        );
        let request =
            build_target_subscribe_request(&target, CommitmentLevel::Confirmed, None).unwrap();

        let filter = &request.transactions["tx_filter"];
        assert_eq!(filter.account_include.len(), 1);
        assert_eq!(
            filter.account_include[0],
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );
    }

    #[test]
    fn test_build_target_subscribe_request_account() {
        let target = make_target(
            TargetKind::Account,
            Some("8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC"),
            "solana-mainnet",
        );
        let request =
            build_target_subscribe_request(&target, CommitmentLevel::Confirmed, None).unwrap();

        let filter = &request.transactions["tx_filter"];
        assert_eq!(filter.account_include.len(), 1);
        assert_eq!(
            filter.account_include[0],
            "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC"
        );
    }

    #[test]
    fn test_build_target_subscribe_request_with_from_slot() {
        let target = make_target(
            TargetKind::Wallet,
            Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"),
            "solana-mainnet",
        );
        let request =
            build_target_subscribe_request(&target, CommitmentLevel::Finalized, Some(99999))
                .unwrap();

        assert_eq!(request.from_slot, Some(99999));
        assert_eq!(request.commitment, Some(CommitmentLevel::Finalized as i32));
    }

    #[test]
    fn test_build_target_subscribe_request_no_address() {
        let target = make_target(TargetKind::Wallet, None, "solana-mainnet");
        let result = build_target_subscribe_request(&target, CommitmentLevel::Confirmed, None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must have an address"));
    }

    // -- V2 RawTransaction conversion --

    #[test]
    fn test_convert_grpc_to_raw_transaction_basic() {
        let account_keys = vec![vec![1u8; 32], vec![2u8; 32]];
        let tx_info = make_test_tx_info(
            vec![10_000_000_000, 0],
            vec![9_500_000_000, 500_000_000],
            account_keys,
        );

        let result = convert_grpc_to_raw_transaction(
            &tx_info,
            12345,
            "solana-mainnet",
            "solana-grpc-wallet-backfill",
        );
        assert!(result.is_ok());

        let raw_tx = result.unwrap();
        assert!(!raw_tx.tx_hash.is_empty());
        assert_eq!(raw_tx.network, "solana-mainnet");
        assert_eq!(raw_tx.source, "solana-grpc-wallet-backfill");
        assert_eq!(raw_tx.block_number, Some(12345));
        assert!(raw_tx.ingestion_run_id.is_none());

        // Verify no wallet_address or user_id in serialized output
        let serialized = serde_json::to_string(&raw_tx).unwrap();
        assert!(!serialized.contains("wallet_address"));
        assert!(!serialized.contains("user_id"));
    }

    #[test]
    fn test_convert_grpc_to_raw_transaction_program_source() {
        let tx_info = make_test_tx_info(vec![0], vec![0], vec![vec![0u8; 32]]);
        let result = convert_grpc_to_raw_transaction(
            &tx_info,
            500,
            "solana-mainnet",
            "solana-grpc-program-backfill",
        );
        assert!(result.is_ok());

        let raw_tx = result.unwrap();
        assert_eq!(raw_tx.source, "solana-grpc-program-backfill");
        assert_eq!(raw_tx.network, "solana-mainnet");
    }

    #[test]
    fn test_convert_grpc_to_raw_transaction_account_source() {
        let tx_info = make_test_tx_info(vec![0], vec![0], vec![vec![0u8; 32]]);
        let result = convert_grpc_to_raw_transaction(
            &tx_info,
            700,
            "solana-mainnet",
            "solana-grpc-account-stream",
        );
        assert!(result.is_ok());

        let raw_tx = result.unwrap();
        assert_eq!(raw_tx.source, "solana-grpc-account-stream");
        assert_eq!(raw_tx.block_number, Some(700));
    }

    #[test]
    fn test_convert_grpc_to_raw_transaction_preserves_metadata() {
        let account_keys = vec![vec![1u8; 32]];
        let tx_info = make_test_tx_info(vec![1_000_000_000], vec![999_000_000], account_keys);

        let raw_tx = convert_grpc_to_raw_transaction(
            &tx_info,
            42,
            "solana-mainnet",
            "solana-grpc-wallet-backfill",
        )
        .unwrap();

        assert_eq!(raw_tx.raw_metadata["slot"], 42);
        assert_eq!(raw_tx.raw_metadata["meta"]["fee"], 5000);
    }

    // -- Cursor parsing --

    #[test]
    fn test_cursor_to_from_slot_none() {
        assert_eq!(cursor_to_from_slot(None), None);
    }

    #[test]
    fn test_cursor_to_from_slot_with_last_slot() {
        let cursor = json!({"last_slot": 42000u64, "last_signature": "abc123"});
        assert_eq!(cursor_to_from_slot(Some(&cursor)), Some(42000));
    }

    #[test]
    fn test_cursor_to_from_slot_missing_key() {
        let cursor = json!({"other": "value"});
        assert_eq!(cursor_to_from_slot(Some(&cursor)), None);
    }

    #[test]
    fn test_cursor_to_from_slot_wrong_type() {
        let cursor = json!({"last_slot": "not_a_number"});
        assert_eq!(cursor_to_from_slot(Some(&cursor)), None);
    }

    // -- Source string construction --

    #[test]
    fn test_v2_source_strings() {
        assert_eq!(
            v2_source(TargetKind::Wallet, "backfill"),
            "solana-grpc-wallet-backfill"
        );
        assert_eq!(
            v2_source(TargetKind::Program, "backfill"),
            "solana-grpc-program-backfill"
        );
        assert_eq!(
            v2_source(TargetKind::Account, "backfill"),
            "solana-grpc-account-backfill"
        );
        assert_eq!(
            v2_source(TargetKind::Wallet, "stream"),
            "solana-grpc-wallet-stream"
        );
        assert_eq!(
            v2_source(TargetKind::Program, "stream"),
            "solana-grpc-program-stream"
        );
        assert_eq!(
            v2_source(TargetKind::Account, "stream"),
            "solana-grpc-account-stream"
        );
    }

    // -- Legacy V1 conversion preserved --

    #[test]
    fn test_legacy_v1_conversion_still_works() {
        let tx_info = make_test_tx_info(vec![1000], vec![500], vec![vec![1u8; 32]]);
        let v1_tx = convert_grpc_transaction(&tx_info, 100).unwrap();
        assert!(matches!(v1_tx.chain, Chain::Solana));
        assert_eq!(v1_tx.wallet_address, "");
        assert_eq!(v1_tx.user_id, Uuid::nil());
    }

    // -- Subscription request correctness per target kind --

    #[test]
    fn test_wallet_target_does_not_use_program_ids() {
        let target = make_target(
            TargetKind::Wallet,
            Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"),
            "solana-mainnet",
        );
        let request =
            build_target_subscribe_request(&target, CommitmentLevel::Confirmed, None).unwrap();
        let filter = &request.transactions["tx_filter"];

        // The wallet target should ONLY include the wallet pubkey,
        // NOT the default program IDs from the legacy path.
        assert_eq!(filter.account_include.len(), 1);
        assert_eq!(
            filter.account_include[0],
            "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"
        );
        for default_pid in DEFAULT_PROGRAM_IDS {
            assert!(
                !filter.account_include.contains(&default_pid.to_string()),
                "wallet target should not include default program ID {}",
                default_pid
            );
        }
    }

    #[test]
    fn test_program_target_does_not_stamp_wallet_identity() {
        // Program target subscription should not include any wallet address
        let target = make_target(
            TargetKind::Program,
            Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            "solana-mainnet",
        );
        let request =
            build_target_subscribe_request(&target, CommitmentLevel::Confirmed, None).unwrap();
        let filter = &request.transactions["tx_filter"];

        assert_eq!(filter.account_include.len(), 1);
        assert_eq!(
            filter.account_include[0],
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );

        // Verify the resulting RawTransaction also has no wallet identity
        let tx_info = make_test_tx_info(vec![0], vec![0], vec![vec![0u8; 32]]);
        let raw_tx = convert_grpc_to_raw_transaction(
            &tx_info,
            100,
            "solana-mainnet",
            "solana-grpc-program-backfill",
        )
        .unwrap();
        let serialized = serde_json::to_string(&raw_tx).unwrap();
        assert!(!serialized.contains("wallet_address"));
        assert!(!serialized.contains("user_id"));
    }

    #[test]
    fn test_account_target_uses_account_pubkey() {
        let pda = "8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC";
        let target = make_target(TargetKind::Account, Some(pda), "solana-mainnet");
        let request =
            build_target_subscribe_request(&target, CommitmentLevel::Confirmed, None).unwrap();
        let filter = &request.transactions["tx_filter"];

        assert_eq!(filter.account_include.len(), 1);
        assert_eq!(filter.account_include[0], pda);
    }

    // -- Construction from NetworkContext --

    #[test]
    fn test_from_network_context() {
        use spectraplex_core::config::{NetworkConfig, ProviderConfig};
        use spectraplex_core::provider::{NetworkId, ProviderRegistry};

        let mut networks = std::collections::HashMap::new();
        networks.insert(
            "solana-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );

        let providers = vec![ProviderConfig {
            network: "solana-mainnet".to_string(),
            kind: "grpc".to_string(),
            url: "https://yellowstone.example.com".to_string(),
            priority: Some(1),
            capabilities: vec!["stream".to_string()],
            token_env: None,
            token: Some("my-grpc-token".to_string()),
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("solana-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &net).unwrap();
        let adapter = SolanaGrpcAdapter::from_network_context(&ctx).unwrap();

        assert_eq!(adapter.config.endpoint, "https://yellowstone.example.com");
        assert_eq!(adapter.config.x_token.as_deref(), Some("my-grpc-token"));
    }

    #[test]
    fn test_from_network_context_no_stream_fails() {
        use spectraplex_core::config::{NetworkConfig, ProviderConfig};
        use spectraplex_core::provider::{NetworkId, ProviderRegistry};

        let mut networks = std::collections::HashMap::new();
        networks.insert(
            "solana-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );

        // Only historical, no stream
        let providers = vec![ProviderConfig {
            network: "solana-mainnet".to_string(),
            kind: "rpc".to_string(),
            url: "https://api.mainnet-beta.solana.com".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("solana-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &net).unwrap();
        let result = SolanaGrpcAdapter::from_network_context(&ctx);

        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("stream"));
    }
}
