use core::models::{Chain, EventType, TaxEvent, ChainIngestor};
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::prelude::{
    SubscribeRequest, SubscribeRequestFilterTransactions, CommitmentLevel, SubscribeUpdateTransaction
};
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status::TransactionTokenBalance;
use futures::{StreamExt, SinkExt};
use std::str::FromStr;
use std::collections::HashMap;

pub struct SolanaGrpcAdapter {
    endpoint: String,
    x_token: Option<String>,
}

impl SolanaGrpcAdapter {
    pub fn new(endpoint: &str, x_token: Option<String>) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            x_token,
        }
    }

    // Maps the raw gRPC protobuf message to our universal TaxEvent
    fn parse_grpc_tx(&self, tx_update: SubscribeUpdateTransaction, target_wallet: &str) -> Vec<TaxEvent> {
        let mut events = Vec::new();
        
        // 1. Unpack the Transaction Meta
        let meta = match tx_update.meta {
            Some(m) => m,
            None => return vec![], // Skipped failed or missing meta
        };

        // 2. Unpack Signature (Tx Hash)
        let signature = bs58::encode(&tx_update.signature).into_string();
        
        // 3. Logic: Compare Pre/Post Token Balances
        // The gRPC proto definitions for TokenBalance are slightly different than JSON RPC
        // We iterate through post_token_balances and find the matching pre_token_balance
        
        for post in &meta.post_token_balances {
            if post.owner == target_wallet {
                // Find matching pre-balance
                let pre_amount = meta.pre_token_balances.iter()
                   .find(|pre| pre.account_index == post.account_index)
                   .map(|pre| pre.ui_token_amount.clone().unwrap().ui_amount)
                   .unwrap_or(0.0);

                let post_amount = post.ui_token_amount.clone().unwrap().ui_amount;
                let delta = post_amount - pre_amount;

                if delta.abs() > 0.000001 {
                    events.push(TaxEvent {
                        chain: Chain::Solana,
                        tx_hash: signature.clone(),
                        block_time: 0, // Note: gRPC tx update often doesn't have blocktime attached directly, might need to join with Block update or use slot time
                        event_type: if delta > 0.0 { EventType::TransferIn } else { EventType::TransferOut },
                        asset_symbol: "UNKNOWN_SPL".to_string(), // Need external metadata lookup
                        asset_address: post.mint.clone(),
                        amount_change: delta,
                        fee_paid: 0.0, // Simplification for prototype
                        fee_asset: "SOL".to_string(),
                    });
                }
            }
        }

        // TODO: Handle Native SOL changes (requires looking at AccountKeys + pre/post balances)

        events
    }
}

#[async_trait::async_trait]
impl ChainIngestor for SolanaGrpcAdapter {
    // Note: This function now STREAMs data and prints it, rather than returning a static Vec.
    // In a real app, you might pass a callback or a channel sender here.
    async fn fetch_history(&self, wallet: &str, _limit: usize) -> anyhow::Result<Vec<TaxEvent>> {
        println!("Connecting to Yellowstone gRPC at {}...", self.endpoint);

        // 1. Connect
        let mut client = GeyserGrpcClient::connect(self.endpoint.clone(), self.x_token.clone(), None)?;
        
        // 2. Create Subscription Request
        let mut transactions_filter = HashMap::new();
        transactions_filter.insert(
            "wallet_watch".to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: vec![wallet.to_string()], // Watch transactions involving this wallet
                account_exclude: vec![],
                account_required: vec![],
            },
        );

        let request = SubscribeRequest {
            transactions: transactions_filter,
            commitment: Some(CommitmentLevel::Confirmed as i32),
           ..Default::default()
        };

        // 3. Subscribe
        let (mut subscribe_tx, mut stream) = client.subscribe().await?;
        subscribe_tx.send(request).await?;

        println!("Listening for live transactions for {}...", wallet);

        // 4. Consume Stream
        while let Some(message) = stream.next().await {
            match message {
                Ok(msg) => {
                    // The message is an `UpdateOneof` enum
                    if let Some(update) = msg.update_oneof {
                        match update {
                            yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof::Transaction(tx_update) => {
                                let events = self.parse_grpc_tx(tx_update, wallet);
                                for event in events {
                                    println!("Found Tax Event: {:?} {} {}", event.event_type, event.amount_change, event.asset_address);
                                    // In a real app, you'd send this to your DB channel here
                                }
                            },
                            _ => {} // Ignore heartbeats/slots/etc
                        }
                    }
                }
                Err(error) => {
                    eprintln!("Stream error: {:?}", error);
                    break;
                }
            }
        }

        Ok(vec![]) // Returns empty because we are streaming indefinitely
    }
}