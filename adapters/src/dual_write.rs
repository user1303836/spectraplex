//! Dual-write compatibility layer (P1-W4).
//!
//! Provides V1 → V2 conversion functions and orchestration so every wallet
//! ingestion writes both V1 tables (`transactions`, `indexer_checkpoints`) and
//! V2 tables (`raw_transactions`, `target_matches`, `checkpoints`).
//!
//! V2 writes are best-effort: failures are logged but never abort the V1 path.

use chrono::Utc;
use spectraplex_core::models::{Chain, IndexerCheckpoint, Transaction};
use spectraplex_core::v2::{
    ChainFamily, Checkpoint, IndexTarget, RawTransaction, TargetKind, TargetMatch, TargetMode,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::repo::Repository;

// ---------------------------------------------------------------------------
// Chain → Network / Source mapping  (Rollout Plan Section 2.4)
// ---------------------------------------------------------------------------

/// Map a V1 `Chain` to the default V2 network identifier.
///
/// Matches Rollout Plan Section 2.4 exactly:
/// - `Solana`       → `"solana-mainnet"`
/// - `Ethereum`     → `"ethereum-mainnet"`
/// - `Hyperliquid`  → `"hypercore-mainnet"`
pub fn chain_to_default_network(chain: &Chain) -> &'static str {
    match chain {
        Chain::Solana => "solana-mainnet",
        Chain::Ethereum => "ethereum-mainnet",
        Chain::Hyperliquid => "hypercore-mainnet",
    }
}

/// Map a V1 `Chain` to the default V2 ingestion source.
///
/// Matches Rollout Plan Section 2.4 exactly:
/// - `Solana`       → `"rpc"`
/// - `Ethereum`     → `"rpc"`
/// - `Hyperliquid`  → `"rest"`
pub fn chain_to_default_source(chain: &Chain) -> &'static str {
    match chain {
        Chain::Solana => "rpc",
        Chain::Ethereum => "rpc",
        Chain::Hyperliquid => "rest",
    }
}

// ---------------------------------------------------------------------------
// V1 → V2 conversion functions
// ---------------------------------------------------------------------------

/// Convert a V1 `Transaction` to a V2 `RawTransaction`.
///
/// Per Rollout Plan Section 2.2:
/// - `user_id` and `wallet_address` are stripped (target-agnostic)
/// - `chain` maps to `network` via `chain_to_default_network`
/// - `source` maps via `chain_to_default_source`
/// - `block_number` is extracted from `raw_metadata` (Solana: `slot`, EVM: `block_number`)
/// - `id`, `timestamp`, `tx_hash`, `raw_metadata` are copied directly
pub fn v1_tx_to_v2_raw(tx: &Transaction) -> RawTransaction {
    let block_number = extract_block_number(&tx.chain, &tx.raw_metadata);

    RawTransaction {
        id: tx.id,
        network: chain_to_default_network(&tx.chain).to_string(),
        tx_hash: tx.tx_hash.clone(),
        timestamp: tx.timestamp,
        block_number,
        raw_metadata: tx.raw_metadata.clone(),
        source: chain_to_default_source(&tx.chain).to_string(),
        ingestion_run_id: None,
        ingested_at: Utc::now(),
    }
}

/// Extract `block_number` from raw_metadata based on chain.
///
/// - Solana: `raw_metadata["slot"]`
/// - Ethereum: `raw_metadata["block_number"]`
/// - Hyperliquid: None (blockless chain)
fn extract_block_number(chain: &Chain, metadata: &serde_json::Value) -> Option<i64> {
    match chain {
        Chain::Solana => metadata.get("slot").and_then(|v| v.as_i64()),
        Chain::Ethereum => metadata.get("block_number").and_then(|v| v.as_i64()),
        Chain::Hyperliquid => None,
    }
}

/// Convert a V1 `IndexerCheckpoint` to a V2 `Checkpoint`.
///
/// Per P0-W3 Section 5 / Rollout Plan Section 2.3:
/// - Builds a JSONB cursor from the flat V1 fields
/// - Solana cursor: `{ last_signature, last_slot }`
/// - EVM cursor: `{ last_block }`
/// - HyperCore cursor: `{ last_timestamp }` (raw seconds)
pub fn v1_checkpoint_to_v2(cp: &IndexerCheckpoint, target_id: Uuid) -> Checkpoint {
    let network = chain_to_default_network(&cp.chain).to_string();
    let source = chain_to_default_source(&cp.chain).to_string();

    let cursor = build_v2_cursor(cp);

    Checkpoint {
        id: Uuid::new_v4(),
        target_id,
        network,
        source,
        cursor,
        updated_at: Utc::now(),
    }
}

/// Build the JSONB cursor from V1 checkpoint fields.
///
/// Per P0-W3 Sections 5.2–5.5 and Section 9.4:
/// - Solana: `{ "last_signature": ..., "last_slot": ... }`
/// - EVM: `{ "last_block": ... }`
/// - HyperCore: `{ "last_timestamp": ... }` (raw seconds)
///
/// Only non-None fields are included.
fn build_v2_cursor(cp: &IndexerCheckpoint) -> serde_json::Value {
    let mut cursor = serde_json::Map::new();

    match cp.chain {
        Chain::Solana => {
            if let Some(ref sig) = cp.last_signature {
                cursor.insert(
                    "last_signature".to_string(),
                    serde_json::Value::String(sig.clone()),
                );
            }
            if let Some(slot) = cp.last_slot {
                cursor.insert(
                    "last_slot".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(slot)),
                );
            }
        }
        Chain::Ethereum => {
            if let Some(block) = cp.last_block {
                cursor.insert(
                    "last_block".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(block)),
                );
            }
        }
        Chain::Hyperliquid => {
            if let Some(ts) = cp.last_timestamp {
                cursor.insert(
                    "last_timestamp".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(ts)),
                );
            }
        }
    }

    serde_json::Value::Object(cursor)
}

/// Build `TargetMatch` rows linking a target to a set of raw transaction IDs.
///
/// All matches use `match_reason = "sender"` since V1 wallet ingestion finds
/// transactions where the wallet is the sender/signer.
pub fn build_target_matches(target_id: Uuid, raw_tx_ids: &[Uuid]) -> Vec<TargetMatch> {
    let now = Utc::now();
    raw_tx_ids
        .iter()
        .map(|&raw_tx_id| TargetMatch {
            id: Uuid::new_v4(),
            target_id,
            raw_transaction_id: raw_tx_id,
            match_reason: Some("sender".to_string()),
            matched_at: now,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ensure_wallet_target: idempotent wallet target creation
// ---------------------------------------------------------------------------

impl Repository {
    /// Look up or create an `IndexTarget` for the given wallet address.
    ///
    /// Idempotent: if a target with `(kind=Wallet, network, address)` already
    /// exists, it is returned. Otherwise a new one is created with
    /// `mode=Both` and `chain_family` derived from the chain.
    pub async fn ensure_wallet_target(
        &self,
        chain: &Chain,
        wallet_address: &str,
        owner_id: Option<Uuid>,
    ) -> anyhow::Result<IndexTarget> {
        let network = chain_to_default_network(chain);
        let chain_family = ChainFamily::from(chain.clone());

        // Check if target already exists
        if let Some(existing) = self
            .get_index_target_by_address(TargetKind::Wallet, network, wallet_address)
            .await?
        {
            return Ok(existing);
        }

        // Create new target
        let now = Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: network.to_string(),
            chain_family,
            address: Some(wallet_address.to_string()),
            filter_spec: None,
            mode: TargetMode::Both,
            label: None,
            owner_id,
            created_at: now,
            updated_at: now,
        };

        self.create_index_target(&target).await?;
        info!(
            target_id = %target.id,
            wallet = %wallet_address,
            network = %network,
            "Created wallet IndexTarget via compatibility shim"
        );
        Ok(target)
    }

    // -----------------------------------------------------------------------
    // Dual-write orchestration
    // -----------------------------------------------------------------------

    /// Save transactions using both V1 and V2 paths.
    ///
    /// V1 write (`save_transactions`) executes first. V2 writes
    /// (`save_raw_transactions` + `save_target_matches`) are best-effort:
    /// failures are logged but do not fail the operation.
    pub async fn save_transactions_dual_write(
        &self,
        txs: &[Transaction],
        target_id: Uuid,
    ) -> anyhow::Result<()> {
        // V1 write (authoritative)
        self.save_transactions(txs).await?;

        // V2 write (best-effort)
        if let Err(e) = self.v2_write_transactions(txs, target_id).await {
            warn!(
                error = %e,
                target_id = %target_id,
                count = txs.len(),
                "V2 dual-write for transactions failed (V1 write succeeded)"
            );
        }

        Ok(())
    }

    /// Save a checkpoint using both V1 and V2 paths.
    ///
    /// V1 write (`save_checkpoint`) executes first. V2 write
    /// (`upsert_checkpoint_v2`) is best-effort.
    pub async fn save_checkpoint_dual_write(
        &self,
        checkpoint: &IndexerCheckpoint,
        target_id: Uuid,
    ) -> anyhow::Result<()> {
        // V1 write (authoritative)
        self.save_checkpoint(checkpoint).await?;

        // V2 write (best-effort)
        let v2_cp = v1_checkpoint_to_v2(checkpoint, target_id);
        if let Err(e) = self.upsert_checkpoint_v2(&v2_cp).await {
            warn!(
                error = %e,
                target_id = %target_id,
                "V2 dual-write for checkpoint failed (V1 write succeeded)"
            );
        }

        Ok(())
    }

    /// Combined dual-write for transactions and checkpoint.
    ///
    /// V1 path uses `save_transactions_and_checkpoint` (atomic). V2 writes
    /// are best-effort and run after V1 succeeds.
    pub async fn save_transactions_and_checkpoint_dual_write(
        &self,
        txs: &[Transaction],
        checkpoint: &IndexerCheckpoint,
        target_id: Uuid,
    ) -> anyhow::Result<()> {
        // V1 write (authoritative, atomic)
        self.save_transactions_and_checkpoint(txs, checkpoint)
            .await?;

        // V2 write (best-effort)
        if let Err(e) = self.v2_write_transactions(txs, target_id).await {
            warn!(
                error = %e,
                target_id = %target_id,
                count = txs.len(),
                "V2 dual-write for transactions failed (V1 write succeeded)"
            );
        }

        let v2_cp = v1_checkpoint_to_v2(checkpoint, target_id);
        if let Err(e) = self.upsert_checkpoint_v2(&v2_cp).await {
            warn!(
                error = %e,
                target_id = %target_id,
                "V2 dual-write for checkpoint failed (V1 write succeeded)"
            );
        }

        Ok(())
    }

    /// Internal: convert and write V2 raw_transactions + target_matches.
    async fn v2_write_transactions(
        &self,
        txs: &[Transaction],
        target_id: Uuid,
    ) -> anyhow::Result<()> {
        if txs.is_empty() {
            return Ok(());
        }

        let v2_txs: Vec<RawTransaction> = txs.iter().map(v1_tx_to_v2_raw).collect();
        self.save_raw_transactions(&v2_txs).await?;

        let raw_tx_ids: Vec<Uuid> = v2_txs.iter().map(|rt| rt.id).collect();
        let matches = build_target_matches(target_id, &raw_tx_ids);
        self.save_target_matches(&matches).await?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use spectraplex_core::models::Chain;

    // -- chain_to_default_network (Rollout Plan Section 2.4) --

    #[test]
    fn chain_to_network_solana() {
        assert_eq!(chain_to_default_network(&Chain::Solana), "solana-mainnet");
    }

    #[test]
    fn chain_to_network_ethereum() {
        assert_eq!(
            chain_to_default_network(&Chain::Ethereum),
            "ethereum-mainnet"
        );
    }

    #[test]
    fn chain_to_network_hyperliquid() {
        assert_eq!(
            chain_to_default_network(&Chain::Hyperliquid),
            "hypercore-mainnet"
        );
    }

    // -- chain_to_default_source (Rollout Plan Section 2.4) --

    #[test]
    fn chain_to_source_solana() {
        assert_eq!(chain_to_default_source(&Chain::Solana), "rpc");
    }

    #[test]
    fn chain_to_source_ethereum() {
        assert_eq!(chain_to_default_source(&Chain::Ethereum), "rpc");
    }

    #[test]
    fn chain_to_source_hyperliquid() {
        assert_eq!(chain_to_default_source(&Chain::Hyperliquid), "rest");
    }

    // -- V1 → V2 Transaction conversion --

    fn make_solana_tx() -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string(),
            timestamp: 1700000000,
            tx_hash: "5VERv8NMhzg".to_string(),
            chain: Chain::Solana,
            raw_metadata: serde_json::json!({"slot": 298412345, "fee": 5000}),
        }
    }

    fn make_eth_tx() -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
            timestamp: 1700000000,
            tx_hash: "0xdeadbeef".to_string(),
            chain: Chain::Ethereum,
            raw_metadata: serde_json::json!({"block_number": 18000000, "gasUsed": "21000"}),
        }
    }

    fn make_hl_tx() -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "0xhlwallet".to_string(),
            timestamp: 1700000000,
            tx_hash: "hlhash123".to_string(),
            chain: Chain::Hyperliquid,
            raw_metadata: serde_json::json!({"tid": 12345}),
        }
    }

    #[test]
    fn v1_to_v2_solana_strips_user_and_wallet() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx);

        // Verify user_id and wallet_address are not present in serialized form
        let json = serde_json::to_string(&v2).unwrap();
        assert!(!json.contains("user_id"));
        assert!(!json.contains("wallet_address"));
    }

    #[test]
    fn v1_to_v2_solana_maps_network() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx);
        assert_eq!(v2.network, "solana-mainnet");
        assert_eq!(v2.source, "rpc");
    }

    #[test]
    fn v1_to_v2_solana_extracts_block_number_from_slot() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx);
        assert_eq!(v2.block_number, Some(298412345));
    }

    #[test]
    fn v1_to_v2_ethereum_maps_network() {
        let tx = make_eth_tx();
        let v2 = v1_tx_to_v2_raw(&tx);
        assert_eq!(v2.network, "ethereum-mainnet");
        assert_eq!(v2.source, "rpc");
    }

    #[test]
    fn v1_to_v2_ethereum_extracts_block_number() {
        let tx = make_eth_tx();
        let v2 = v1_tx_to_v2_raw(&tx);
        assert_eq!(v2.block_number, Some(18000000));
    }

    #[test]
    fn v1_to_v2_hyperliquid_maps_network() {
        let tx = make_hl_tx();
        let v2 = v1_tx_to_v2_raw(&tx);
        assert_eq!(v2.network, "hypercore-mainnet");
        assert_eq!(v2.source, "rest");
    }

    #[test]
    fn v1_to_v2_hyperliquid_no_block_number() {
        let tx = make_hl_tx();
        let v2 = v1_tx_to_v2_raw(&tx);
        assert_eq!(v2.block_number, None);
    }

    #[test]
    fn v1_to_v2_preserves_id_hash_timestamp_metadata() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx);
        assert_eq!(v2.id, tx.id);
        assert_eq!(v2.tx_hash, tx.tx_hash);
        assert_eq!(v2.timestamp, tx.timestamp);
        assert_eq!(v2.raw_metadata, tx.raw_metadata);
    }

    #[test]
    fn v1_to_v2_ingestion_run_id_is_none() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx);
        assert!(v2.ingestion_run_id.is_none());
    }

    // -- V1 → V2 Checkpoint conversion --

    #[test]
    fn checkpoint_solana_cursor_shape() {
        let cp = IndexerCheckpoint {
            chain: Chain::Solana,
            wallet_address: "wallet123".to_string(),
            last_signature: Some("5VERv8NMhzg".to_string()),
            last_slot: Some(298412345),
            last_block: None,
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();
        let v2 = v1_checkpoint_to_v2(&cp, target_id);

        assert_eq!(v2.target_id, target_id);
        assert_eq!(v2.network, "solana-mainnet");
        assert_eq!(v2.source, "rpc");

        // Verify cursor JSONB shape per P0-W3 Section 5.2
        assert_eq!(v2.cursor["last_signature"], "5VERv8NMhzg");
        assert_eq!(v2.cursor["last_slot"], 298412345);
        // Solana cursor should not have last_block or last_timestamp
        assert!(v2.cursor.get("last_block").is_none());
        assert!(v2.cursor.get("last_timestamp").is_none());
    }

    #[test]
    fn checkpoint_ethereum_cursor_shape() {
        let cp = IndexerCheckpoint {
            chain: Chain::Ethereum,
            wallet_address: "0xwallet".to_string(),
            last_signature: Some("0xbbb".to_string()),
            last_slot: None,
            last_block: Some(21500000),
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();
        let v2 = v1_checkpoint_to_v2(&cp, target_id);

        assert_eq!(v2.network, "ethereum-mainnet");
        assert_eq!(v2.source, "rpc");

        // Verify cursor JSONB shape per P0-W3 Section 5.3
        assert_eq!(v2.cursor["last_block"], 21500000);
        // EVM cursor should not have last_signature or last_slot
        assert!(v2.cursor.get("last_signature").is_none());
        assert!(v2.cursor.get("last_slot").is_none());
    }

    #[test]
    fn checkpoint_hyperliquid_cursor_shape() {
        let cp = IndexerCheckpoint {
            chain: Chain::Hyperliquid,
            wallet_address: "0xhl".to_string(),
            last_signature: Some("hlhash".to_string()),
            last_slot: None,
            last_block: None,
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();
        let v2 = v1_checkpoint_to_v2(&cp, target_id);

        assert_eq!(v2.network, "hypercore-mainnet");
        assert_eq!(v2.source, "rest");

        // Verify cursor JSONB shape per P0-W3 Section 5.4
        // last_timestamp stores raw seconds (ms conversion is post-P1-W4)
        assert_eq!(v2.cursor["last_timestamp"], 1700000000_i64);
        // HL cursor should not have last_signature or last_slot
        assert!(v2.cursor.get("last_signature").is_none());
        assert!(v2.cursor.get("last_slot").is_none());
    }

    #[test]
    fn checkpoint_solana_missing_optional_fields() {
        let cp = IndexerCheckpoint {
            chain: Chain::Solana,
            wallet_address: "wallet".to_string(),
            last_signature: None,
            last_slot: None,
            last_block: None,
            last_timestamp: None,
        };
        let v2 = v1_checkpoint_to_v2(&cp, Uuid::new_v4());
        // Cursor should be an empty object when no fields are present
        assert_eq!(v2.cursor, serde_json::json!({}));
    }

    #[test]
    fn checkpoint_hyperliquid_missing_timestamp() {
        let cp = IndexerCheckpoint {
            chain: Chain::Hyperliquid,
            wallet_address: "wallet".to_string(),
            last_signature: None,
            last_slot: None,
            last_block: None,
            last_timestamp: None,
        };
        let v2 = v1_checkpoint_to_v2(&cp, Uuid::new_v4());
        assert!(v2.cursor.get("last_timestamp").is_none());
    }

    // -- Target match construction --

    #[test]
    fn build_target_matches_creates_correct_count() {
        let target_id = Uuid::new_v4();
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let matches = build_target_matches(target_id, &ids);
        assert_eq!(matches.len(), 5);
    }

    #[test]
    fn build_target_matches_uses_sender_reason() {
        let target_id = Uuid::new_v4();
        let ids = vec![Uuid::new_v4()];
        let matches = build_target_matches(target_id, &ids);
        assert_eq!(matches[0].match_reason, Some("sender".to_string()));
    }

    #[test]
    fn build_target_matches_links_correct_ids() {
        let target_id = Uuid::new_v4();
        let raw_id = Uuid::new_v4();
        let matches = build_target_matches(target_id, &[raw_id]);
        assert_eq!(matches[0].target_id, target_id);
        assert_eq!(matches[0].raw_transaction_id, raw_id);
    }

    #[test]
    fn build_target_matches_unique_ids() {
        let target_id = Uuid::new_v4();
        let ids: Vec<Uuid> = (0..10).map(|_| Uuid::new_v4()).collect();
        let matches = build_target_matches(target_id, &ids);
        let match_ids: std::collections::HashSet<Uuid> = matches.iter().map(|m| m.id).collect();
        assert_eq!(match_ids.len(), 10, "each match should have a unique id");
    }

    #[test]
    fn build_target_matches_empty_input() {
        let matches = build_target_matches(Uuid::new_v4(), &[]);
        assert!(matches.is_empty());
    }

    // -- extract_block_number --

    #[test]
    fn extract_block_number_solana_slot() {
        let meta = serde_json::json!({"slot": 100, "fee": 5000});
        assert_eq!(extract_block_number(&Chain::Solana, &meta), Some(100));
    }

    #[test]
    fn extract_block_number_solana_missing_slot() {
        let meta = serde_json::json!({"fee": 5000});
        assert_eq!(extract_block_number(&Chain::Solana, &meta), None);
    }

    #[test]
    fn extract_block_number_ethereum() {
        let meta = serde_json::json!({"block_number": 18000000});
        assert_eq!(
            extract_block_number(&Chain::Ethereum, &meta),
            Some(18000000)
        );
    }

    #[test]
    fn extract_block_number_ethereum_missing() {
        let meta = serde_json::json!({"gasUsed": "21000"});
        assert_eq!(extract_block_number(&Chain::Ethereum, &meta), None);
    }

    #[test]
    fn extract_block_number_hyperliquid_always_none() {
        let meta = serde_json::json!({"tid": 12345});
        assert_eq!(extract_block_number(&Chain::Hyperliquid, &meta), None);
    }

    // -- Batch conversion consistency --

    #[test]
    fn batch_conversion_preserves_order_and_ids() {
        let txs: Vec<Transaction> = (0..5)
            .map(|i| Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                wallet_address: "wallet".to_string(),
                timestamp: 1700000000 + i,
                tx_hash: format!("hash{i}"),
                chain: Chain::Solana,
                raw_metadata: serde_json::json!({"slot": 100 + i}),
            })
            .collect();

        let v2_txs: Vec<RawTransaction> = txs.iter().map(v1_tx_to_v2_raw).collect();
        assert_eq!(v2_txs.len(), 5);

        for (v1, v2) in txs.iter().zip(v2_txs.iter()) {
            assert_eq!(v1.id, v2.id);
            assert_eq!(v1.tx_hash, v2.tx_hash);
            assert_eq!(v1.timestamp, v2.timestamp);
        }
    }

    // -- Full dual-write batch assembly --

    #[test]
    fn dual_write_batch_assembly() {
        let target_id = Uuid::new_v4();
        let txs: Vec<Transaction> = (0..3)
            .map(|i| Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                wallet_address: "wallet".to_string(),
                timestamp: 1700000000 + i,
                tx_hash: format!("hash{i}"),
                chain: Chain::Ethereum,
                raw_metadata: serde_json::json!({"block_number": 18000000 + i}),
            })
            .collect();

        let v2_txs: Vec<RawTransaction> = txs.iter().map(v1_tx_to_v2_raw).collect();
        let raw_ids: Vec<Uuid> = v2_txs.iter().map(|rt| rt.id).collect();
        let matches = build_target_matches(target_id, &raw_ids);

        assert_eq!(v2_txs.len(), 3);
        assert_eq!(matches.len(), 3);

        for (v2_tx, tm) in v2_txs.iter().zip(matches.iter()) {
            assert_eq!(tm.target_id, target_id);
            assert_eq!(tm.raw_transaction_id, v2_tx.id);
            assert_eq!(v2_tx.network, "ethereum-mainnet");
            assert_eq!(v2_tx.source, "rpc");
        }
    }
}
