use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

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

use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, Transaction};

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
}
