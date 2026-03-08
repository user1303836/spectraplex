//! Compatibility layer for bridging V1 wallet-centric adapters to the V2
//! target-driven `Connector` interface.
//!
//! This module provides:
//!
//! - `LegacyConnectorAdapter`: wraps a V1 `ChainIngestor` so it can be used
//!   as a V2 `Connector` for wallet targets.
//! - `legacy_wallet_target()`: synthesizes an `IndexTarget` from the legacy
//!   (chain, wallet, user_id) parameters so existing CLI/API paths can route
//!   through the new interface.
//! - `v1_checkpoint_to_cursor()` / `cursor_to_v1_checkpoint()`: convert between
//!   the V1 `IndexerCheckpoint` and the V2 opaque cursor JSON.

use chrono::Utc;
use spectraplex_core::connector::{Connector, ConnectorCapabilities};
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint};
use spectraplex_core::v2::{
    ChainFamily, IndexTarget, IngestionBatch, RawTransaction, TargetKind, TargetMode,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Legacy wallet target construction
// ---------------------------------------------------------------------------

/// Map a V1 `Chain` enum to a (ChainFamily, network) pair.
fn chain_to_family_and_network(chain: &Chain) -> (ChainFamily, &'static str) {
    match chain {
        Chain::Solana => (ChainFamily::Solana, "solana-mainnet"),
        Chain::Ethereum => (ChainFamily::Evm, "ethereum-mainnet"),
        Chain::Hyperliquid => (ChainFamily::Hyperliquid, "hypercore-mainnet"),
    }
}

/// Map a chain string (as used in CLI) to a V1 `Chain` enum.
fn chain_str_to_enum(chain: &str) -> Option<Chain> {
    match chain {
        "solana" => Some(Chain::Solana),
        "ethereum" => Some(Chain::Ethereum),
        "hyperliquid" => Some(Chain::Hyperliquid),
        _ => None,
    }
}

/// Synthesize a V2 `IndexTarget` from legacy (chain, wallet, user_id) parameters.
///
/// This allows existing CLI/API wallet-based calls to be expressed as target
/// specs and routed through the V2 connector interface.
pub fn legacy_wallet_target(
    chain: &str,
    wallet: &str,
    owner_id: Option<Uuid>,
) -> Option<IndexTarget> {
    let chain_enum = chain_str_to_enum(chain)?;
    let (family, network) = chain_to_family_and_network(&chain_enum);
    let normalized_address = match family {
        ChainFamily::Evm | ChainFamily::Hyperliquid => {
            spectraplex_core::v2::normalize_evm_address(wallet)
        }
        ChainFamily::Solana => spectraplex_core::v2::normalize_solana_address(wallet),
    };
    let now = Utc::now();
    Some(IndexTarget {
        id: Uuid::new_v4(),
        kind: TargetKind::Wallet,
        network: network.to_string(),
        chain_family: family,
        address: Some(normalized_address),
        filter_spec: None,
        mode: TargetMode::Both,
        label: Some(format!("Legacy wallet: {}", wallet)),
        owner_id,
        created_at: now,
        updated_at: now,
    })
}

/// Build a legacy `IndexTarget` from a `Chain` enum instead of a chain string.
pub fn legacy_wallet_target_from_chain(
    chain: &Chain,
    wallet: &str,
    owner_id: Option<Uuid>,
) -> IndexTarget {
    let (family, network) = chain_to_family_and_network(chain);
    let normalized_address = match family {
        ChainFamily::Evm | ChainFamily::Hyperliquid => {
            spectraplex_core::v2::normalize_evm_address(wallet)
        }
        ChainFamily::Solana => spectraplex_core::v2::normalize_solana_address(wallet),
    };
    let now = Utc::now();
    IndexTarget {
        id: Uuid::new_v4(),
        kind: TargetKind::Wallet,
        network: network.to_string(),
        chain_family: family,
        address: Some(normalized_address),
        filter_spec: None,
        mode: TargetMode::Both,
        label: Some(format!("Legacy wallet: {}", wallet)),
        owner_id,
        created_at: now,
        updated_at: now,
    }
}

// ---------------------------------------------------------------------------
// Checkpoint conversion
// ---------------------------------------------------------------------------

/// Convert a V1 `IndexerCheckpoint` to a V2 opaque cursor JSON.
pub fn v1_checkpoint_to_cursor(cp: &IndexerCheckpoint) -> serde_json::Value {
    serde_json::json!({
        "v1_compat": true,
        "last_signature": cp.last_signature,
        "last_slot": cp.last_slot,
        "last_block": cp.last_block,
        "last_timestamp": cp.last_timestamp,
    })
}

/// Convert a V2 opaque cursor JSON back to a V1 `IndexerCheckpoint`.
///
/// Returns `None` if the cursor is not a V1-compatible checkpoint.
pub fn cursor_to_v1_checkpoint(
    chain: &str,
    wallet: &str,
    cursor: &serde_json::Value,
) -> Option<IndexerCheckpoint> {
    if !cursor.get("v1_compat")?.as_bool()? {
        return None;
    }
    let chain_enum = chain_str_to_enum(chain)?;
    Some(IndexerCheckpoint {
        chain: chain_enum,
        wallet_address: wallet.to_string(),
        last_signature: cursor
            .get("last_signature")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        last_slot: cursor.get("last_slot").and_then(|v| v.as_i64()),
        last_block: cursor.get("last_block").and_then(|v| v.as_i64()),
        last_timestamp: cursor.get("last_timestamp").and_then(|v| v.as_i64()),
    })
}

// ---------------------------------------------------------------------------
// LegacyConnectorAdapter
// ---------------------------------------------------------------------------

/// Wraps a V1 `ChainIngestor` to present it as a V2 `Connector`.
///
/// Only wallet targets are supported. The adapter:
/// 1. Extracts the wallet address from the `IndexTarget`
/// 2. Converts the V2 cursor to a V1 checkpoint
/// 3. Calls `ChainIngestor::fetch_history`
/// 4. Converts the V1 `Transaction` results to V2 `RawTransaction`s
pub struct LegacyConnectorAdapter<I: ChainIngestor + Send + Sync> {
    ingestor: I,
    chain_family: ChainFamily,
    network: String,
    chain_str: String,
}

impl<I: ChainIngestor + Send + Sync> LegacyConnectorAdapter<I> {
    pub fn new(ingestor: I, chain: &Chain) -> Self {
        let (family, network) = chain_to_family_and_network(chain);
        let chain_str = match chain {
            Chain::Solana => "solana",
            Chain::Ethereum => "ethereum",
            Chain::Hyperliquid => "hyperliquid",
        };
        Self {
            ingestor,
            chain_family: family,
            network: network.to_string(),
            chain_str: chain_str.to_string(),
        }
    }
}

/// Convert a V1 `Transaction` to a V2 `RawTransaction`.
fn v1_tx_to_raw(tx: &spectraplex_core::models::Transaction, network: &str) -> RawTransaction {
    RawTransaction {
        id: Uuid::new_v4(),
        network: network.to_string(),
        tx_hash: tx.tx_hash.clone(),
        timestamp: tx.timestamp,
        block_number: tx
            .raw_metadata
            .get("block_number")
            .and_then(|v| v.as_i64())
            .or_else(|| tx.raw_metadata.get("slot").and_then(|v| v.as_i64())),
        raw_metadata: tx.raw_metadata.clone(),
        source: "legacy-compat".to_string(),
        ingestion_run_id: None,
        ingested_at: Utc::now(),
    }
}

#[async_trait::async_trait]
impl<I: ChainIngestor + Send + Sync> Connector for LegacyConnectorAdapter<I> {
    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supported_target_kinds: vec![TargetKind::Wallet],
            supported_modes: vec![TargetMode::Backfill],
            chain_family: self.chain_family,
        }
    }

    async fn backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        // Validate target kind
        if target.kind != TargetKind::Wallet {
            anyhow::bail!(
                "LegacyConnectorAdapter only supports wallet targets, got {:?}",
                target.kind
            );
        }

        // Extract wallet address
        let wallet = target
            .address
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("wallet target must have an address"))?;

        // Convert cursor to V1 checkpoint
        let v1_checkpoint =
            cursor.and_then(|c| cursor_to_v1_checkpoint(&self.chain_str, wallet, c));

        // Use a nil user_id since V2 does not embed consumer identity in Bronze
        let user_id = target.owner_id.unwrap_or(Uuid::nil());

        // Call the legacy ingestor
        let v1_txs = self
            .ingestor
            .fetch_history(wallet, limit, user_id, v1_checkpoint.as_ref())
            .await?;

        // Convert V1 transactions to V2 raw transactions
        let records: Vec<RawTransaction> = v1_txs
            .iter()
            .map(|tx| v1_tx_to_raw(tx, &self.network))
            .collect();

        Ok(IngestionBatch {
            records,
            checkpoint: None,
            run_metadata: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use spectraplex_core::connector::validate_target;
    use spectraplex_core::models::{Chain, Transaction};

    // -- legacy_wallet_target --

    #[test]
    fn legacy_wallet_target_solana() {
        let target = legacy_wallet_target(
            "solana",
            "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
            None,
        )
        .unwrap();
        assert_eq!(target.kind, TargetKind::Wallet);
        assert_eq!(target.chain_family, ChainFamily::Solana);
        assert_eq!(target.network, "solana-mainnet");
        assert_eq!(
            target.address.as_deref(),
            Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy")
        );
        assert!(target.filter_spec.is_none());
        assert_eq!(target.mode, TargetMode::Both);
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn legacy_wallet_target_ethereum() {
        let target = legacy_wallet_target(
            "ethereum",
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            Some(Uuid::nil()),
        )
        .unwrap();
        assert_eq!(target.kind, TargetKind::Wallet);
        assert_eq!(target.chain_family, ChainFamily::Evm);
        assert_eq!(target.network, "ethereum-mainnet");
        // EVM address should be normalized to lowercase
        assert_eq!(
            target.address.as_deref(),
            Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
        );
        assert_eq!(target.owner_id, Some(Uuid::nil()));
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn legacy_wallet_target_hyperliquid() {
        let target = legacy_wallet_target(
            "hyperliquid",
            "0x742d35Cc6634C0532925a3b844Bc9e7595f2bD18",
            None,
        )
        .unwrap();
        assert_eq!(target.kind, TargetKind::Wallet);
        assert_eq!(target.chain_family, ChainFamily::Hyperliquid);
        assert_eq!(target.network, "hypercore-mainnet");
        assert_eq!(
            target.address.as_deref(),
            Some("0x742d35cc6634c0532925a3b844bc9e7595f2bd18")
        );
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn legacy_wallet_target_unknown_chain() {
        assert!(legacy_wallet_target("bitcoin", "addr", None).is_none());
    }

    // -- legacy_wallet_target_from_chain --

    #[test]
    fn legacy_wallet_target_from_chain_solana() {
        let target = legacy_wallet_target_from_chain(&Chain::Solana, "abc", None);
        assert_eq!(target.kind, TargetKind::Wallet);
        assert_eq!(target.chain_family, ChainFamily::Solana);
        assert_eq!(target.network, "solana-mainnet");
    }

    #[test]
    fn legacy_wallet_target_from_chain_ethereum() {
        let target = legacy_wallet_target_from_chain(&Chain::Ethereum, "0xABC", None);
        assert_eq!(target.chain_family, ChainFamily::Evm);
        assert_eq!(target.address.as_deref(), Some("0xabc"));
    }

    // -- Checkpoint conversion --

    #[test]
    fn v1_checkpoint_to_cursor_roundtrip() {
        let cp = IndexerCheckpoint {
            chain: Chain::Solana,
            wallet_address: "abc".to_string(),
            last_signature: Some("sig123".to_string()),
            last_slot: Some(42000),
            last_block: None,
            last_timestamp: Some(1700000000),
        };
        let cursor = v1_checkpoint_to_cursor(&cp);
        assert_eq!(cursor["v1_compat"], true);
        assert_eq!(cursor["last_signature"], "sig123");
        assert_eq!(cursor["last_slot"], 42000);
        assert!(cursor["last_block"].is_null());
        assert_eq!(cursor["last_timestamp"], 1700000000);

        let back = cursor_to_v1_checkpoint("solana", "abc", &cursor).unwrap();
        assert!(matches!(back.chain, Chain::Solana));
        assert_eq!(back.wallet_address, "abc");
        assert_eq!(back.last_signature, Some("sig123".to_string()));
        assert_eq!(back.last_slot, Some(42000));
        assert_eq!(back.last_block, None);
        assert_eq!(back.last_timestamp, Some(1700000000));
    }

    #[test]
    fn v1_checkpoint_ethereum_roundtrip() {
        let cp = IndexerCheckpoint {
            chain: Chain::Ethereum,
            wallet_address: "0xabc".to_string(),
            last_signature: Some("0xhash".to_string()),
            last_slot: None,
            last_block: Some(18_000_000),
            last_timestamp: Some(1700000000),
        };
        let cursor = v1_checkpoint_to_cursor(&cp);
        let back = cursor_to_v1_checkpoint("ethereum", "0xabc", &cursor).unwrap();
        assert!(matches!(back.chain, Chain::Ethereum));
        assert_eq!(back.last_block, Some(18_000_000));
    }

    #[test]
    fn cursor_to_v1_checkpoint_non_compat() {
        let cursor = serde_json::json!({"some": "other"});
        assert!(cursor_to_v1_checkpoint("solana", "abc", &cursor).is_none());
    }

    #[test]
    fn cursor_to_v1_checkpoint_unknown_chain() {
        let cursor = serde_json::json!({"v1_compat": true, "last_signature": null, "last_slot": null, "last_block": null, "last_timestamp": null});
        assert!(cursor_to_v1_checkpoint("bitcoin", "abc", &cursor).is_none());
    }

    // -- v1_tx_to_raw --

    #[test]
    fn v1_tx_converts_to_raw() {
        let tx = Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet123".to_string(),
            timestamp: 1700000000,
            tx_hash: "sig_abc".to_string(),
            chain: Chain::Solana,
            raw_metadata: serde_json::json!({"slot": 42000}),
        };
        let raw = v1_tx_to_raw(&tx, "solana-mainnet");
        assert_eq!(raw.network, "solana-mainnet");
        assert_eq!(raw.tx_hash, "sig_abc");
        assert_eq!(raw.timestamp, 1700000000);
        assert_eq!(raw.block_number, Some(42000));
        assert_eq!(raw.source, "legacy-compat");
        assert!(raw.ingestion_run_id.is_none());
        // Verify no wallet_address or user_id in raw
        let json = serde_json::to_string(&raw).unwrap();
        assert!(!json.contains("wallet_address"));
        assert!(!json.contains("user_id"));
    }

    #[test]
    fn v1_tx_converts_evm_block_number() {
        let tx = Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "0xwallet".to_string(),
            timestamp: 1700000000,
            tx_hash: "0xhash".to_string(),
            chain: Chain::Ethereum,
            raw_metadata: serde_json::json!({"block_number": 18_000_000}),
        };
        let raw = v1_tx_to_raw(&tx, "ethereum-mainnet");
        assert_eq!(raw.block_number, Some(18_000_000));
    }

    #[test]
    fn v1_tx_no_block_number() {
        let tx = Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "0xwallet".to_string(),
            timestamp: 1700000000,
            tx_hash: "hash1".to_string(),
            chain: Chain::Hyperliquid,
            raw_metadata: serde_json::json!({"type": "fill"}),
        };
        let raw = v1_tx_to_raw(&tx, "hypercore-mainnet");
        assert_eq!(raw.block_number, None);
    }

    // -- LegacyConnectorAdapter capabilities --

    struct MockIngestor;

    #[async_trait::async_trait]
    impl ChainIngestor for MockIngestor {
        async fn fetch_history(
            &self,
            _wallet: &str,
            limit: usize,
            _user_id: Uuid,
            _checkpoint: Option<&IndexerCheckpoint>,
        ) -> anyhow::Result<Vec<Transaction>> {
            let mut txs = Vec::new();
            for i in 0..limit.min(3) {
                txs.push(Transaction {
                    id: Uuid::new_v4(),
                    user_id: Uuid::nil(),
                    wallet_address: "mock_wallet".to_string(),
                    timestamp: 1700000000 + i as i64,
                    tx_hash: format!("hash_{}", i),
                    chain: Chain::Solana,
                    raw_metadata: serde_json::json!({"slot": 1000 + i}),
                });
            }
            Ok(txs)
        }
    }

    #[test]
    fn legacy_adapter_capabilities() {
        let adapter = LegacyConnectorAdapter::new(MockIngestor, &Chain::Solana);
        let caps = adapter.capabilities();
        assert_eq!(caps.chain_family, ChainFamily::Solana);
        assert_eq!(caps.supported_target_kinds, vec![TargetKind::Wallet]);
        assert_eq!(caps.supported_modes, vec![TargetMode::Backfill]);
    }

    #[tokio::test]
    async fn legacy_adapter_backfill_wallet() {
        let adapter = LegacyConnectorAdapter::new(MockIngestor, &Chain::Solana);
        let target = legacy_wallet_target("solana", "TestWallet123", None).unwrap();

        let batch = adapter.backfill(&target, None, 5).await.unwrap();
        assert_eq!(batch.records.len(), 3); // MockIngestor caps at 3
        assert_eq!(batch.records[0].network, "solana-mainnet");
        assert_eq!(batch.records[0].tx_hash, "hash_0");
        assert_eq!(batch.records[0].source, "legacy-compat");
    }

    #[tokio::test]
    async fn legacy_adapter_rejects_non_wallet() {
        let adapter = LegacyConnectorAdapter::new(MockIngestor, &Chain::Solana);
        let now = Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Program,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: Some("TokenkegQ".to_string()),
            filter_spec: None,
            mode: TargetMode::Backfill,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };

        let result = adapter.backfill(&target, None, 5).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("only supports wallet"));
    }

    #[tokio::test]
    async fn legacy_adapter_backfill_with_cursor() {
        let adapter = LegacyConnectorAdapter::new(MockIngestor, &Chain::Solana);
        let target = legacy_wallet_target("solana", "TestWallet123", None).unwrap();

        let cursor = serde_json::json!({
            "v1_compat": true,
            "last_signature": "prev_sig",
            "last_slot": 999,
            "last_block": null,
            "last_timestamp": 1699999999
        });

        let batch = adapter.backfill(&target, Some(&cursor), 5).await.unwrap();
        // MockIngestor doesn't actually use the checkpoint, but we verify it doesn't error
        assert_eq!(batch.records.len(), 3);
    }

    #[tokio::test]
    async fn legacy_adapter_backfill_missing_address() {
        let adapter = LegacyConnectorAdapter::new(MockIngestor, &Chain::Solana);
        let now = Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: None,
            filter_spec: None,
            mode: TargetMode::Both,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };

        let result = adapter.backfill(&target, None, 5).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must have an address"));
    }
}
