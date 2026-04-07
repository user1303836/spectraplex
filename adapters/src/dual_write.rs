//! Dual-write compatibility layer (P1-W4).
//!
//! Provides V1 → V2 conversion functions and orchestration so every wallet
//! ingestion writes both V1 tables (`transactions`, `indexer_checkpoints`) and
//! V2 tables (`raw_transactions`, `target_matches`, `checkpoints`).
//!
//! V2 writes are best-effort: failures are logged but never abort the V1 path.

use chrono::Utc;
use spectraplex_core::materializer::{DatasetName, DatasetRegistry};
use spectraplex_core::models::{Chain, IndexerCheckpoint, Transaction};
use spectraplex_core::v2::{
    ChainFamily, Checkpoint, CompletenessStatus, DatasetCompleteness, DatasetVersion,
    DatasetVersionStatus, IndexTarget, RawTransaction, TargetKind, TargetMatch, TargetMode,
};
use std::collections::{HashMap, HashSet};
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
// Network resolution for normalize path
// ---------------------------------------------------------------------------

/// Map a V1 `Chain` enum to its V2 `ChainFamily` for disambiguation.
fn chain_to_family(chain: &Chain) -> ChainFamily {
    match chain {
        Chain::Solana => ChainFamily::Solana,
        Chain::Ethereum => ChainFamily::Evm,
        Chain::Hyperliquid => ChainFamily::Hyperliquid,
    }
}

/// Resolve the effective network for a V1 transaction during Silver
/// materialization.
///
/// Priority:
/// 1. `explicit_network` — caller-provided override (e.g. from API/CLI request)
/// 2. `bronze_network_map` — actual network from existing Bronze `raw_transactions`
///    rows, disambiguated by chain family when a tx_hash exists on multiple networks
/// 3. `chain_to_default_network()` — last resort, derived from V1 `Chain` enum
///
/// The third path is a lossy fallback: `Chain::Ethereum` maps to
/// `"ethereum-mainnet"` even for L2/sidechain transactions that were originally
/// ingested as `"base-mainnet"` or `"arbitrum-mainnet"`.  Returns `true` in
/// the second tuple element when the chain default was used (so callers can log
/// a warning).
pub fn resolve_effective_network(
    tx: &Transaction,
    explicit_network: Option<&str>,
    bronze_network_map: &HashMap<String, Vec<String>>,
) -> (String, bool) {
    if let Some(n) = explicit_network {
        return (n.to_string(), false);
    }
    if let Some(networks) = bronze_network_map.get(&tx.tx_hash) {
        if networks.len() == 1 {
            return (networks[0].clone(), false);
        }
        // Multiple networks for the same tx_hash — disambiguate by chain family.
        let expected_family = chain_to_family(&tx.chain);
        let family_matches: Vec<&String> = networks
            .iter()
            .filter(|n| DatasetRegistry::chain_family_for_network(n) == Some(expected_family))
            .collect();
        if family_matches.len() == 1 {
            return (family_matches[0].clone(), false);
        }
        // Same-family ambiguity (e.g., same hash on ethereum-mainnet and
        // base-mainnet).  This is NOT authoritative — we cannot determine
        // the correct network without explicit context.  Mark as inferred
        // so callers can skip or warn rather than silently attaching Silver
        // rows to the wrong network.
        if !family_matches.is_empty() {
            // Return deterministic pick but flag as inferred.
            let default = chain_to_default_network(&tx.chain);
            let pick = family_matches
                .iter()
                .find(|n| n.as_str() == default)
                .or_else(|| family_matches.iter().min())
                .unwrap();
            return ((*pick).clone(), true);
        }
        // No family match at all — cross-family only.  Pick deterministically
        // but flag as inferred.
        if let Some(first) = networks.iter().min() {
            return (first.clone(), true);
        }
    }
    (chain_to_default_network(&tx.chain).to_string(), true)
}

// ---------------------------------------------------------------------------
// V1 → V2 conversion functions
// ---------------------------------------------------------------------------

/// Convert a V1 `Transaction` to a V2 `RawTransaction`.
///
/// Per Rollout Plan Section 2.2:
/// - `user_id` and `wallet_address` are stripped (target-agnostic)
/// - `chain` maps to `network` via `chain_to_default_network` (unless overridden)
/// - `source` maps via `chain_to_default_source`
/// - `block_number` is extracted from `raw_metadata` (Solana: `slot`, EVM: `block_number`)
/// - `id`, `timestamp`, `tx_hash`, `raw_metadata` are copied directly
///
/// When `explicit_network` is `Some`, it overrides the chain-derived default.
/// This is critical for EVM networks like `base-mainnet` and `arbitrum-mainnet`
/// that all map to `Chain::Ethereum` but need distinct network identifiers in
/// the V2 `raw_transactions` table.
pub fn v1_tx_to_v2_raw(tx: &Transaction, explicit_network: Option<&str>) -> RawTransaction {
    let block_number = extract_block_number(&tx.chain, &tx.raw_metadata);
    let network = match explicit_network {
        Some(n) => n.to_string(),
        None => chain_to_default_network(&tx.chain).to_string(),
    };

    RawTransaction {
        id: tx.id,
        network,
        tx_hash: tx.tx_hash.clone(),
        timestamp: tx.timestamp,
        block_number,
        raw_metadata: tx.raw_metadata.clone(),
        source: chain_to_default_source(&tx.chain).to_string(),
        ingestion_run_id: None,
        ingested_at: Utc::now(),
    }
}

/// Convert a V2 `RawTransaction` back to a V1 `Transaction` for compatibility
/// projection.
///
/// Used in the V2-authoritative ingestion path to write best-effort V1
/// `transactions` rows so downstream consumers that still read the V1 schema
/// continue to work during the migration window.
pub fn v2_raw_to_v1_tx(
    raw: &RawTransaction,
    wallet: &str,
    chain: Chain,
    user_id: Option<Uuid>,
) -> Transaction {
    // Reuse raw.id as the V1 Transaction.id. v1_tx_to_v2_raw copies
    // tx.id -> raw.id during dual-write, so this round-trips correctly
    // and matches the compat transactions row already persisted by the
    // backfill worker. This also makes retries idempotent since the
    // same raw.id always produces the same ledger entry IDs.
    Transaction {
        id: raw.id,
        user_id: user_id.unwrap_or_else(|| Uuid::new_v5(&Uuid::NAMESPACE_URL, wallet.as_bytes())),
        wallet_address: wallet.to_string(),
        timestamp: raw.timestamp,
        tx_hash: raw.tx_hash.clone(),
        chain,
        raw_metadata: raw.raw_metadata.clone(),
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
    v1_checkpoint_to_v2_with_network(cp, target_id, None)
}

/// Convert a V1 checkpoint to V2, with an optional explicit network override.
///
/// When `explicit_network` is `Some`, it is used instead of the chain-derived
/// default. This is critical for EVM networks like `base-mainnet` and
/// `arbitrum-mainnet` that all map to `Chain::Ethereum` but need distinct
/// checkpoint rows in the V2 `checkpoints` table.
pub fn v1_checkpoint_to_v2_with_network(
    cp: &IndexerCheckpoint,
    target_id: Uuid,
    explicit_network: Option<&str>,
) -> Checkpoint {
    let network = match explicit_network {
        Some(n) => n.to_string(),
        None => chain_to_default_network(&cp.chain).to_string(),
    };
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

/// Convert a V2 `Checkpoint` back to a V1 `IndexerCheckpoint`.
///
/// This is used in the network-first resume path: when an explicit network is
/// provided, the V2 checkpoint is authoritative and is converted to V1 format
/// so the existing adapter `fetch_history` methods can consume it.
pub fn v2_checkpoint_to_v1(
    v2: &Checkpoint,
    chain: &Chain,
    wallet_address: &str,
) -> IndexerCheckpoint {
    let cursor = &v2.cursor;

    let last_signature = cursor
        .get("last_signature")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let last_slot = cursor.get("last_slot").and_then(|v| v.as_i64());

    let last_block = cursor.get("last_block").and_then(|v| v.as_i64());

    let last_timestamp = cursor.get("last_timestamp").and_then(|v| v.as_i64());

    IndexerCheckpoint {
        chain: chain.clone(),
        wallet_address: wallet_address.to_string(),
        last_signature,
        last_slot,
        last_block,
        last_timestamp,
    }
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
        self.ensure_wallet_target_inner(
            network,
            ChainFamily::from(chain.clone()),
            wallet_address,
            owner_id,
        )
        .await
    }

    /// Like `ensure_wallet_target`, but uses an explicit network ID instead of
    /// deriving it from `Chain`. This is needed for EVM networks like
    /// `base-mainnet` and `arbitrum-mainnet` that all map to `Chain::Ethereum`
    /// but must have distinct target rows.
    pub async fn ensure_wallet_target_for_network(
        &self,
        network: &str,
        chain: &Chain,
        wallet_address: &str,
        owner_id: Option<Uuid>,
    ) -> anyhow::Result<IndexTarget> {
        self.ensure_wallet_target_inner(
            network,
            ChainFamily::from(chain.clone()),
            wallet_address,
            owner_id,
        )
        .await
    }

    async fn ensure_wallet_target_inner(
        &self,
        network: &str,
        chain_family: ChainFamily,
        wallet_address: &str,
        owner_id: Option<Uuid>,
    ) -> anyhow::Result<IndexTarget> {
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
    ///
    /// When `explicit_network` is provided, V2 raw transactions are stamped
    /// with the given network instead of the chain-derived default.
    pub async fn save_transactions_dual_write(
        &self,
        txs: &[Transaction],
        target_id: Uuid,
        explicit_network: Option<&str>,
    ) -> anyhow::Result<()> {
        // V1 write (authoritative)
        self.save_transactions(txs).await?;

        // V2 write (best-effort)
        if let Err(e) = self
            .v2_write_transactions(txs, target_id, explicit_network)
            .await
        {
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
    ///
    /// When `explicit_network` is provided it overrides the chain-derived
    /// default in the V2 checkpoint, ensuring different EVM networks
    /// get distinct checkpoint rows.
    pub async fn save_checkpoint_dual_write(
        &self,
        checkpoint: &IndexerCheckpoint,
        target_id: Uuid,
        explicit_network: Option<&str>,
    ) -> anyhow::Result<()> {
        // V1 write (authoritative)
        self.save_checkpoint(checkpoint).await?;

        // V2 write (best-effort)
        let v2_cp = v1_checkpoint_to_v2_with_network(checkpoint, target_id, explicit_network);
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
    ///
    /// When `explicit_network` is provided it overrides the chain-derived
    /// default in the V2 checkpoint.
    pub async fn save_transactions_and_checkpoint_dual_write(
        &self,
        txs: &[Transaction],
        checkpoint: &IndexerCheckpoint,
        target_id: Uuid,
        explicit_network: Option<&str>,
    ) -> anyhow::Result<()> {
        // V1 write (authoritative, atomic)
        self.save_transactions_and_checkpoint(txs, checkpoint)
            .await?;

        // V2 write (best-effort)
        if let Err(e) = self
            .v2_write_transactions(txs, target_id, explicit_network)
            .await
        {
            warn!(
                error = %e,
                target_id = %target_id,
                count = txs.len(),
                "V2 dual-write for transactions failed (V1 write succeeded)"
            );
        }

        let v2_cp = v1_checkpoint_to_v2_with_network(checkpoint, target_id, explicit_network);
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
    ///
    /// EVM ingestion can emit multiple V1 rows per on-chain transaction (one
    /// per matching log), so the input may contain duplicate `(network, tx_hash)`
    /// pairs.  PostgreSQL's `ON CONFLICT DO UPDATE` rejects touching the same
    /// row twice in a single statement, so we deduplicate before upserting and
    /// then map every *original* transaction back to its canonical ID for the
    /// target_matches.
    ///
    /// When `explicit_network` is provided, it overrides the chain-derived
    /// default in the V2 raw transactions.
    async fn v2_write_transactions(
        &self,
        txs: &[Transaction],
        target_id: Uuid,
        explicit_network: Option<&str>,
    ) -> anyhow::Result<()> {
        if txs.is_empty() {
            return Ok(());
        }

        // 1. Convert all V1 txs to V2 raw transactions.
        let v2_txs: Vec<RawTransaction> = txs
            .iter()
            .map(|tx| v1_tx_to_v2_raw(tx, explicit_network))
            .collect();

        // 2. Deduplicate by (network, tx_hash), keeping the first occurrence.
        let mut seen: HashMap<(&str, &str), usize> = HashMap::new();
        let mut deduped: Vec<&RawTransaction> = Vec::new();
        for tx in &v2_txs {
            let key = (tx.network.as_str(), tx.tx_hash.as_str());
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(key) {
                e.insert(deduped.len());
                deduped.push(tx);
            }
        }

        // 3. Upsert only the unique set and get canonical IDs back.
        let deduped_owned: Vec<RawTransaction> = deduped.into_iter().cloned().collect();
        let canonical_ids = self
            .upsert_raw_transactions_returning_ids(&deduped_owned)
            .await?;

        // 4. Build (network, tx_hash) → canonical ID map from the upsert results.
        let id_map: HashMap<(&str, &str), Uuid> = deduped_owned
            .iter()
            .zip(canonical_ids.iter())
            .map(|(tx, &id)| ((tx.network.as_str(), tx.tx_hash.as_str()), id))
            .collect();

        // 5. Resolve canonical IDs for ALL original transactions (including
        //    duplicates) and create target_matches.
        let all_canonical_ids: Vec<Uuid> = v2_txs
            .iter()
            .filter_map(|tx| {
                id_map
                    .get(&(tx.network.as_str(), tx.tx_hash.as_str()))
                    .copied()
            })
            .collect();

        let matches = build_target_matches(target_id, &all_canonical_ids);
        self.save_target_matches(&matches).await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Silver dataset materialization
    // -----------------------------------------------------------------------

    /// Materialize Silver datasets from V1 transactions.
    ///
    /// For each transaction, resolves the canonical `raw_transactions.id` via a
    /// batch lookup, then extracts chain-specific Silver records (token
    /// transfers, native balance deltas, decoded events, HL fills/funding/
    /// positions) with proper `raw_transaction_id` linking. After writing the
    /// Silver records, creates or bumps dataset versions and updates
    /// completeness metadata so dataset status and export provenance reflect
    /// the newly materialized rows.
    ///
    /// When `explicit_network` is provided, it overrides the chain-derived
    /// default for network lookups and Silver record stamping.
    ///
    /// When `explicit_network` is `None` (the normalize path), the function
    /// consults existing Bronze `raw_transactions` rows to recover the actual
    /// network for each tx_hash.  Only when no Bronze row exists does it fall
    /// back to `chain_to_default_network()` — and logs a warning when it does.
    ///
    /// V2 Silver writes are best-effort: failures are logged but never abort
    /// the V1 normalization path.
    pub async fn materialize_silver_datasets(
        &self,
        txs: &[Transaction],
        explicit_network: Option<&str>,
    ) {
        if txs.is_empty() {
            return;
        }

        // 0. When no explicit network is provided, consult Bronze to recover
        //    the actual network for each tx_hash.  This is critical because V1
        //    `Transaction` rows only carry `chain: Chain` (e.g. `Ethereum`),
        //    which maps to "ethereum-mainnet" by default — but the original
        //    ingest may have been for "base-mainnet" or "arbitrum-mainnet".
        //    Bronze `raw_transactions` rows have the correct `network` field.
        let bronze_network_map: HashMap<String, Vec<String>> = if explicit_network.is_none() {
            let all_hashes: Vec<String> = txs
                .iter()
                .map(|tx| tx.tx_hash.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            match self.lookup_raw_tx_networks_by_hashes(&all_hashes).await {
                Ok(map) => map
                    .into_iter()
                    .map(|(hash, entries)| {
                        let networks: Vec<String> =
                            entries.into_iter().map(|(_id, network)| network).collect();
                        (hash, networks)
                    })
                    .collect(),
                Err(e) => {
                    warn!(
                        error = %e,
                        "Silver materialization: failed to resolve networks from Bronze; \
                         falling back to chain defaults"
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };

        // Pre-resolve networks for all transactions.  Transactions whose network
        // cannot be determined authoritatively (same-family ambiguity or no Bronze
        // row) are excluded — they are skipped rather than attached to a guessed
        // network.  Callers should re-run normalize with --network to provide
        // explicit context for skipped transactions.
        let mut resolved_networks: HashMap<String, String> = HashMap::new(); // tx_hash → network
        let mut skipped = 0usize;
        for tx in txs {
            let (network, was_inferred) =
                resolve_effective_network(tx, explicit_network, &bronze_network_map);
            if was_inferred {
                warn!(
                    tx_hash = %tx.tx_hash,
                    chain = ?tx.chain,
                    guessed_network = %network,
                    "Silver materialization: network for tx_hash is ambiguous or \
                     inferred — skipping this transaction. Re-run normalize with \
                     --network to provide explicit context."
                );
                skipped += 1;
                continue;
            }
            resolved_networks.insert(tx.tx_hash.clone(), network);
        }
        if skipped > 0 {
            warn!(
                skipped,
                total = txs.len(),
                "Silver materialization: skipped transactions with ambiguous network"
            );
        }

        // Only proceed with transactions that have authoritative network identity.
        let effective_network =
            |tx: &Transaction| -> Option<String> { resolved_networks.get(&tx.tx_hash).cloned() };

        // 1. Batch-resolve canonical raw_transactions.id for each (network, tx_hash).
        let pairs: Vec<(String, String)> = txs
            .iter()
            .filter_map(|tx| effective_network(tx).map(|n| (n, tx.tx_hash.clone())))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        let mut raw_id_map = match self.lookup_raw_transaction_ids(&pairs).await {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    error = %e,
                    "Silver materialization: failed to resolve raw_transaction_ids"
                );
                HashMap::new()
            }
        };

        // 1b. Self-healing: for any V1-only transactions that have no
        //     corresponding V2 raw_transactions row, create the V2 rows now
        //     so Silver records always have provenance.
        let missing_txs: Vec<&Transaction> = txs
            .iter()
            .filter(|tx| {
                if let Some(net) = effective_network(tx) {
                    let key = (net, tx.tx_hash.clone());
                    !raw_id_map.contains_key(&key)
                } else {
                    false // skipped tx — don't back-fill
                }
            })
            .collect();

        if !missing_txs.is_empty() {
            // For self-healing backfill, use the resolved effective network
            // (which already consulted Bronze) rather than just the chain default.
            let v2_rows: Vec<RawTransaction> = missing_txs
                .iter()
                .filter_map(|tx| effective_network(tx).map(|net| v1_tx_to_v2_raw(tx, Some(&net))))
                .collect();

            match self.save_raw_transactions(&v2_rows).await {
                Ok(()) => {
                    info!(
                        count = v2_rows.len(),
                        "Silver materialization: back-filled V2 raw_transactions for V1-only rows"
                    );

                    // Re-resolve IDs for the newly inserted rows.
                    let missing_pairs: Vec<(String, String)> = missing_txs
                        .iter()
                        .filter_map(|tx| effective_network(tx).map(|n| (n, tx.tx_hash.clone())))
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect();

                    match self.lookup_raw_transaction_ids(&missing_pairs).await {
                        Ok(new_ids) => {
                            raw_id_map.extend(new_ids);
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                "Silver materialization: failed to re-resolve IDs after back-fill"
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        count = v2_rows.len(),
                        "Silver materialization: failed to back-fill V2 raw_transactions"
                    );
                }
            }
        }

        // 1c. Self-healing: create target_matches for back-filled raw_transactions
        //     so target-scoped dataset queries (which JOIN through target_matches)
        //     can see V1-only data.
        {
            // Group all transactions by (network, wallet_address) to resolve targets.
            let mut wallet_groups: HashMap<(String, String), Vec<&Transaction>> = HashMap::new();
            for tx in txs {
                if let Some(network) = effective_network(tx) {
                    wallet_groups
                        .entry((network, tx.wallet_address.clone()))
                        .or_default()
                        .push(tx);
                }
            }

            for ((network, wallet_address), group_txs) in &wallet_groups {
                // Look up or create the IndexTarget for this wallet.
                let target = match self
                    .get_index_target_by_address(TargetKind::Wallet, network, wallet_address)
                    .await
                {
                    Ok(Some(t)) => t,
                    Ok(None) => {
                        // Derive Chain from network to create the target.
                        let chain = match network.as_str() {
                            "solana-mainnet" => Chain::Solana,
                            "hypercore-mainnet" => Chain::Hyperliquid,
                            _ => Chain::Ethereum,
                        };
                        // Use ensure_wallet_target_for_network to preserve
                        // the explicit network (e.g. base-mainnet) instead of
                        // collapsing back to the chain default.
                        match self
                            .ensure_wallet_target_for_network(network, &chain, wallet_address, None)
                            .await
                        {
                            Ok(t) => t,
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    wallet = %wallet_address,
                                    network = %network,
                                    "Silver materialization: failed to ensure wallet target for target_matches"
                                );
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            wallet = %wallet_address,
                            network = %network,
                            "Silver materialization: failed to look up target for target_matches"
                        );
                        continue;
                    }
                };

                // Collect resolved raw_transaction_ids for this group.
                let raw_tx_ids: Vec<Uuid> = group_txs
                    .iter()
                    .filter_map(|tx| {
                        effective_network(tx).and_then(|net| {
                            let key = (net, tx.tx_hash.clone());
                            raw_id_map.get(&key).copied()
                        })
                    })
                    .collect();

                if raw_tx_ids.is_empty() {
                    continue;
                }

                let matches = build_target_matches(target.id, &raw_tx_ids);
                if let Err(e) = self.save_target_matches(&matches).await {
                    warn!(
                        error = %e,
                        count = matches.len(),
                        wallet = %wallet_address,
                        network = %network,
                        "Silver materialization: failed to back-fill target_matches"
                    );
                } else {
                    info!(
                        count = matches.len(),
                        wallet = %wallet_address,
                        network = %network,
                        "Silver materialization: back-filled target_matches for V1-only rows"
                    );
                }
            }
        }

        // 2. Get or create dataset versions for each dataset that will receive records.
        let dataset_versions = self.resolve_silver_dataset_versions().await;

        // 3. Extract Silver records with resolved raw_tx_id and dataset_version_id.
        let mut all_token_transfers = Vec::new();
        let mut all_native_balance_deltas = Vec::new();
        let mut all_decoded_events = Vec::new();
        let mut all_hl_fills = Vec::new();
        let mut all_hl_funding = Vec::new();
        let mut all_hl_positions = Vec::new();

        for tx in txs {
            let network_str = match effective_network(tx) {
                Some(n) => n,
                None => continue, // skip ambiguous transactions
            };
            let raw_tx_id = raw_id_map
                .get(&(network_str.clone(), tx.tx_hash.clone()))
                .copied();
            let network = network_str.as_str();

            match tx.chain {
                Chain::Solana => {
                    let mut transfers = crate::solana_parser::extract_solana_token_transfers(
                        raw_tx_id,
                        network,
                        &tx.raw_metadata,
                    );
                    stamp_dataset_version_id(&mut transfers, &dataset_versions, "token_transfers");
                    all_token_transfers.extend(transfers);

                    let mut deltas = crate::solana_parser::extract_solana_native_balance_deltas(
                        raw_tx_id,
                        network,
                        &tx.raw_metadata,
                    );
                    stamp_dataset_version_id(
                        &mut deltas,
                        &dataset_versions,
                        "native_balance_deltas",
                    );
                    all_native_balance_deltas.extend(deltas);

                    let mut events = crate::solana_parser::extract_solana_decoded_events(
                        raw_tx_id,
                        network,
                        &tx.raw_metadata,
                    );
                    stamp_dataset_version_id(&mut events, &dataset_versions, "decoded_events");
                    all_decoded_events.extend(events);
                }
                Chain::Ethereum => {
                    let mut transfers = crate::evm_parser::extract_evm_token_transfers(
                        raw_tx_id,
                        network,
                        &tx.raw_metadata,
                    );
                    stamp_dataset_version_id(&mut transfers, &dataset_versions, "token_transfers");
                    all_token_transfers.extend(transfers);

                    let mut events = crate::evm_parser::extract_evm_decoded_events(
                        raw_tx_id,
                        network,
                        &tx.raw_metadata,
                    );
                    stamp_dataset_version_id(&mut events, &dataset_versions, "decoded_events");
                    all_decoded_events.extend(events);
                }
                Chain::Hyperliquid => {
                    let mut transfers =
                        crate::hyperliquid_parser::extract_hyperliquid_token_transfers(
                            raw_tx_id,
                            network,
                            &tx.wallet_address,
                            &tx.raw_metadata,
                        );
                    stamp_dataset_version_id(&mut transfers, &dataset_versions, "token_transfers");
                    all_token_transfers.extend(transfers);

                    let mut deltas =
                        crate::hyperliquid_parser::extract_hyperliquid_native_balance_deltas(
                            raw_tx_id,
                            network,
                            &tx.wallet_address,
                            &tx.raw_metadata,
                        );
                    stamp_dataset_version_id(
                        &mut deltas,
                        &dataset_versions,
                        "native_balance_deltas",
                    );
                    all_native_balance_deltas.extend(deltas);

                    let mut fills = crate::hyperliquid_parser::extract_hl_fill_records(
                        raw_tx_id,
                        network,
                        &tx.raw_metadata,
                    );
                    stamp_dataset_version_id(
                        &mut fills,
                        &dataset_versions,
                        DatasetName::HlFills.as_sql_str(),
                    );
                    all_hl_fills.extend(fills);

                    let mut funding = crate::hyperliquid_parser::extract_hl_funding_payments(
                        raw_tx_id,
                        network,
                        &tx.raw_metadata,
                    );
                    stamp_dataset_version_id(
                        &mut funding,
                        &dataset_versions,
                        DatasetName::HlFunding.as_sql_str(),
                    );
                    all_hl_funding.extend(funding);

                    let mut positions = crate::hyperliquid_parser::extract_hl_position_changes(
                        raw_tx_id,
                        network,
                        &tx.raw_metadata,
                    );
                    stamp_dataset_version_id(
                        &mut positions,
                        &dataset_versions,
                        DatasetName::Positions.as_sql_str(),
                    );
                    all_hl_positions.extend(positions);
                }
            }
        }

        let total = all_token_transfers.len()
            + all_native_balance_deltas.len()
            + all_decoded_events.len()
            + all_hl_fills.len()
            + all_hl_funding.len()
            + all_hl_positions.len();

        if total == 0 {
            return;
        }

        // 4. Write Silver records to the database.
        if !all_token_transfers.is_empty() {
            if let Err(e) = self.save_token_transfers(&all_token_transfers).await {
                warn!(
                    error = %e,
                    count = all_token_transfers.len(),
                    "Silver materialization: token_transfers write failed"
                );
            }
        }

        if !all_native_balance_deltas.is_empty() {
            if let Err(e) = self
                .save_native_balance_deltas(&all_native_balance_deltas)
                .await
            {
                warn!(
                    error = %e,
                    count = all_native_balance_deltas.len(),
                    "Silver materialization: native_balance_deltas write failed"
                );
            }
        }

        if !all_decoded_events.is_empty() {
            if let Err(e) = self.save_decoded_events(&all_decoded_events).await {
                warn!(
                    error = %e,
                    count = all_decoded_events.len(),
                    "Silver materialization: decoded_events write failed"
                );
            }
        }

        if !all_hl_fills.is_empty() {
            if let Err(e) = self.save_hl_fill_records(&all_hl_fills).await {
                warn!(
                    error = %e,
                    count = all_hl_fills.len(),
                    "Silver materialization: hl_fill_records write failed"
                );
            }
        }

        if !all_hl_funding.is_empty() {
            if let Err(e) = self.save_hl_funding_payments(&all_hl_funding).await {
                warn!(
                    error = %e,
                    count = all_hl_funding.len(),
                    "Silver materialization: hl_funding_payments write failed"
                );
            }
        }

        if !all_hl_positions.is_empty() {
            if let Err(e) = self.save_hl_position_changes(&all_hl_positions).await {
                warn!(
                    error = %e,
                    count = all_hl_positions.len(),
                    "Silver materialization: hl_position_changes write failed"
                );
            }
        }

        info!(
            token_transfers = all_token_transfers.len(),
            native_balance_deltas = all_native_balance_deltas.len(),
            decoded_events = all_decoded_events.len(),
            hl_fills = all_hl_fills.len(),
            hl_funding = all_hl_funding.len(),
            hl_positions = all_hl_positions.len(),
            "Silver dataset materialization complete"
        );

        // 5. Update dataset completeness metadata.
        //    Group by (chain, wallet_address) to resolve targets, then update
        //    completeness for each (target, dataset, network) combination.
        self.update_silver_completeness(txs, &dataset_versions, total, &resolved_networks)
            .await;
    }

    /// Materialize Silver datasets directly from Bronze `RawTransaction` rows,
    /// without reconstructing V1 `Transaction` values.
    ///
    /// This is the Bronze-native materialization path used when `ingestion_run_id`
    /// is available. It skips the V1 compatibility layer entirely.
    pub async fn materialize_silver_from_bronze(
        &self,
        raw_txs: &[RawTransaction],
        wallet_address: Option<&str>,
    ) -> usize {
        if raw_txs.is_empty() {
            return 0;
        }

        let dataset_versions = self.resolve_silver_dataset_versions().await;

        let mut all_token_transfers = Vec::new();
        let mut all_native_balance_deltas = Vec::new();
        let mut all_decoded_events = Vec::new();
        let mut all_hl_fills = Vec::new();
        let mut all_hl_funding = Vec::new();
        let mut all_hl_positions = Vec::new();

        for raw in raw_txs {
            let network = raw.network.as_str();
            let raw_tx_id = Some(raw.id);

            if network.starts_with("solana") {
                let mut transfers = crate::solana_parser::extract_solana_token_transfers(
                    raw_tx_id,
                    network,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(&mut transfers, &dataset_versions, "token_transfers");
                all_token_transfers.extend(transfers);

                let mut deltas = crate::solana_parser::extract_solana_native_balance_deltas(
                    raw_tx_id,
                    network,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(&mut deltas, &dataset_versions, "native_balance_deltas");
                all_native_balance_deltas.extend(deltas);

                let mut events = crate::solana_parser::extract_solana_decoded_events(
                    raw_tx_id,
                    network,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(&mut events, &dataset_versions, "decoded_events");
                all_decoded_events.extend(events);
            } else if network.starts_with("hypercore") || network.starts_with("hyperliquid") {
                let wallet = wallet_address.unwrap_or("");
                let mut transfers = crate::hyperliquid_parser::extract_hyperliquid_token_transfers(
                    raw_tx_id,
                    network,
                    wallet,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(&mut transfers, &dataset_versions, "token_transfers");
                all_token_transfers.extend(transfers);

                let mut deltas =
                    crate::hyperliquid_parser::extract_hyperliquid_native_balance_deltas(
                        raw_tx_id,
                        network,
                        wallet,
                        &raw.raw_metadata,
                    );
                stamp_dataset_version_id(&mut deltas, &dataset_versions, "native_balance_deltas");
                all_native_balance_deltas.extend(deltas);

                let mut fills = crate::hyperliquid_parser::extract_hl_fill_records(
                    raw_tx_id,
                    network,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(
                    &mut fills,
                    &dataset_versions,
                    DatasetName::HlFills.as_sql_str(),
                );
                all_hl_fills.extend(fills);

                let mut funding = crate::hyperliquid_parser::extract_hl_funding_payments(
                    raw_tx_id,
                    network,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(
                    &mut funding,
                    &dataset_versions,
                    DatasetName::HlFunding.as_sql_str(),
                );
                all_hl_funding.extend(funding);

                let mut positions = crate::hyperliquid_parser::extract_hl_position_changes(
                    raw_tx_id,
                    network,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(
                    &mut positions,
                    &dataset_versions,
                    DatasetName::Positions.as_sql_str(),
                );
                all_hl_positions.extend(positions);
            } else {
                // EVM (ethereum, base, arbitrum, polygon, etc.)
                let mut transfers = crate::evm_parser::extract_evm_token_transfers(
                    raw_tx_id,
                    network,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(&mut transfers, &dataset_versions, "token_transfers");
                all_token_transfers.extend(transfers);

                let mut events = crate::evm_parser::extract_evm_decoded_events(
                    raw_tx_id,
                    network,
                    &raw.raw_metadata,
                );
                stamp_dataset_version_id(&mut events, &dataset_versions, "decoded_events");
                all_decoded_events.extend(events);
            }
        }

        let total = all_token_transfers.len()
            + all_native_balance_deltas.len()
            + all_decoded_events.len()
            + all_hl_fills.len()
            + all_hl_funding.len()
            + all_hl_positions.len();

        if total == 0 {
            return 0;
        }

        let mut success_count: usize = 0;

        // Write Silver records to the database.
        if !all_token_transfers.is_empty() {
            match self.save_token_transfers(&all_token_transfers).await {
                Ok(()) => {
                    success_count += all_token_transfers.len();
                }
                Err(e) => {
                    warn!(error = %e, count = all_token_transfers.len(), "Bronze-native Silver: token_transfers write failed");
                }
            }
        }
        if !all_native_balance_deltas.is_empty() {
            match self
                .save_native_balance_deltas(&all_native_balance_deltas)
                .await
            {
                Ok(()) => {
                    success_count += all_native_balance_deltas.len();
                }
                Err(e) => {
                    warn!(error = %e, count = all_native_balance_deltas.len(), "Bronze-native Silver: native_balance_deltas write failed");
                }
            }
        }
        if !all_decoded_events.is_empty() {
            match self.save_decoded_events(&all_decoded_events).await {
                Ok(()) => {
                    success_count += all_decoded_events.len();
                }
                Err(e) => {
                    warn!(error = %e, count = all_decoded_events.len(), "Bronze-native Silver: decoded_events write failed");
                }
            }
        }
        if !all_hl_fills.is_empty() {
            match self.save_hl_fill_records(&all_hl_fills).await {
                Ok(()) => {
                    success_count += all_hl_fills.len();
                }
                Err(e) => {
                    warn!(error = %e, count = all_hl_fills.len(), "Bronze-native Silver: hl_fill_records write failed");
                }
            }
        }
        if !all_hl_funding.is_empty() {
            match self.save_hl_funding_payments(&all_hl_funding).await {
                Ok(()) => {
                    success_count += all_hl_funding.len();
                }
                Err(e) => {
                    warn!(error = %e, count = all_hl_funding.len(), "Bronze-native Silver: hl_funding_payments write failed");
                }
            }
        }
        if !all_hl_positions.is_empty() {
            match self.save_hl_position_changes(&all_hl_positions).await {
                Ok(()) => {
                    success_count += all_hl_positions.len();
                }
                Err(e) => {
                    warn!(error = %e, count = all_hl_positions.len(), "Bronze-native Silver: hl_position_changes write failed");
                }
            }
        }

        info!(
            token_transfers = all_token_transfers.len(),
            native_balance_deltas = all_native_balance_deltas.len(),
            decoded_events = all_decoded_events.len(),
            hl_fills = all_hl_fills.len(),
            hl_funding = all_hl_funding.len(),
            hl_positions = all_hl_positions.len(),
            "Bronze-native Silver dataset materialization complete"
        );

        // Track per-dataset record counts for accurate completeness.
        let mut dataset_counts: HashMap<&str, usize> = HashMap::new();
        *dataset_counts.entry("token_transfers").or_default() += all_token_transfers.len();
        *dataset_counts.entry("native_balance_deltas").or_default() +=
            all_native_balance_deltas.len();
        *dataset_counts.entry("decoded_events").or_default() += all_decoded_events.len();
        *dataset_counts
            .entry(DatasetName::HlFills.as_sql_str())
            .or_default() += all_hl_fills.len();
        *dataset_counts
            .entry(DatasetName::HlFunding.as_sql_str())
            .or_default() += all_hl_funding.len();
        *dataset_counts
            .entry(DatasetName::Positions.as_sql_str())
            .or_default() += all_hl_positions.len();

        // Update dataset completeness only for datasets that had records extracted.
        if let Some(wallet) = wallet_address {
            if !wallet.is_empty() {
                let mut net_groups: HashMap<&str, Vec<&RawTransaction>> = HashMap::new();
                for raw in raw_txs {
                    net_groups
                        .entry(raw.network.as_str())
                        .or_default()
                        .push(raw);
                }
                for (network, group) in &net_groups {
                    let target_id = match self
                        .get_index_target_by_address(TargetKind::Wallet, network, wallet)
                        .await
                    {
                        Ok(Some(t)) => t.id,
                        _ => continue,
                    };
                    let coverage_start = group.iter().map(|r| r.timestamp).min();
                    let coverage_end = group.iter().map(|r| r.timestamp).max();
                    let block_start = group.iter().filter_map(|r| r.block_number).min();
                    let block_end = group.iter().filter_map(|r| r.block_number).max();
                    let run_id = group.iter().find_map(|r| r.ingestion_run_id);

                    for (name, vid) in &dataset_versions {
                        let count = dataset_counts.get(name.as_str()).copied().unwrap_or(0);
                        if count == 0 {
                            continue; // Skip datasets with no records for this run.
                        }
                        let dc = DatasetCompleteness {
                            id: Uuid::new_v5(
                                &Uuid::NAMESPACE_URL,
                                format!("{}:{}:{}", target_id, name, network).as_bytes(),
                            ),
                            target_id,
                            dataset_name: name.clone(),
                            dataset_version_id: Some(*vid),
                            network: network.to_string(),
                            status: CompletenessStatus::Partial,
                            coverage_start,
                            coverage_end,
                            block_start,
                            block_end,
                            last_ingestion_run_id: run_id,
                            records_count: count as i64,
                            gap_ranges: None,
                            notes: None,
                            created_at: Utc::now(),
                            updated_at: Utc::now(),
                        };
                        if let Err(e) = self.upsert_dataset_completeness(&dc).await {
                            warn!(
                                error = %e,
                                dataset = %name,
                                network = %network,
                                "Bronze-native: completeness upsert failed"
                            );
                        }
                    }
                }
            }
        }

        success_count
    }

    /// Get or create dataset versions for each Silver dataset.
    ///
    /// Returns a map from canonical dataset name to the active DatasetVersion
    /// id.  Uses `DatasetRegistry::silver_materializable()` as the
    /// authoritative list of Silver datasets that need version tracking.
    async fn resolve_silver_dataset_versions(&self) -> HashMap<String, Uuid> {
        let mut versions = HashMap::new();
        for ds in DatasetRegistry::silver_materializable() {
            let name = ds.as_sql_str();
            match self.get_active_dataset_version(name).await {
                Ok(Some(dv)) => {
                    versions.insert(name.to_string(), dv.id);
                }
                Ok(None) => {
                    // Create initial version
                    let dv = DatasetVersion {
                        id: Uuid::new_v4(),
                        dataset_name: name.to_string(),
                        version: 1,
                        parser_hash: None,
                        created_at: Utc::now(),
                        notes: Some("Auto-created during Silver materialization".to_string()),
                        status: DatasetVersionStatus::Active,
                    };
                    match self.create_dataset_version(&dv).await {
                        Ok(()) => {
                            versions.insert(name.to_string(), dv.id);
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                dataset = %name,
                                "Failed to create dataset version"
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        dataset = %name,
                        "Failed to look up dataset version"
                    );
                }
            }
        }
        versions
    }

    /// Update dataset completeness for each (target, dataset, network) touched
    /// by the materialization run.
    ///
    /// `resolved_networks` provides `tx_hash -> authoritative_network` — only
    /// transactions with deterministic network identity are included.
    async fn update_silver_completeness(
        &self,
        txs: &[Transaction],
        dataset_versions: &HashMap<String, Uuid>,
        total_records: usize,
        resolved_networks: &HashMap<String, String>,
    ) {
        // Group transactions by (network, wallet_address) to resolve targets.
        // Skip transactions with no resolved network (ambiguous).
        let mut wallet_groups: HashMap<(String, String), Vec<&Transaction>> = HashMap::new();
        for tx in txs {
            let network = match resolved_networks.get(&tx.tx_hash) {
                Some(n) => n.clone(),
                None => continue,
            };
            wallet_groups
                .entry((network, tx.wallet_address.clone()))
                .or_default()
                .push(tx);
        }

        for ((network, wallet_address), group_txs) in &wallet_groups {
            let network = network.as_str();

            // Look up the target for this wallet.
            let target_id = match self
                .get_index_target_by_address(TargetKind::Wallet, network, wallet_address)
                .await
            {
                Ok(Some(target)) => target.id,
                Ok(None) => {
                    // No target exists for this wallet; skip completeness update.
                    continue;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        wallet = %wallet_address,
                        network = %network,
                        "Failed to look up target for completeness update"
                    );
                    continue;
                }
            };

            // Compute time coverage from the transactions in this group.
            let coverage_start = group_txs.iter().map(|tx| tx.timestamp).min();
            let coverage_end = group_txs.iter().map(|tx| tx.timestamp).max();
            let block_start = group_txs
                .iter()
                .filter_map(|tx| extract_block_number(&tx.chain, &tx.raw_metadata))
                .min();
            let block_end = group_txs
                .iter()
                .filter_map(|tx| extract_block_number(&tx.chain, &tx.raw_metadata))
                .max();

            // Determine which datasets were populated for this network
            // using the canonical registry.
            let datasets = DatasetRegistry::silver_datasets_for_network(network);

            let now = Utc::now();
            for ds in &datasets {
                let ds_name = ds.as_sql_str();
                let dataset_version_id = dataset_versions.get(ds_name).copied();
                let dc = DatasetCompleteness {
                    id: Uuid::new_v4(),
                    target_id,
                    dataset_name: ds_name.to_string(),
                    dataset_version_id,
                    network: network.to_string(),
                    status: CompletenessStatus::Partial,
                    coverage_start,
                    coverage_end,
                    block_start,
                    block_end,
                    last_ingestion_run_id: None,
                    records_count: total_records as i64,
                    gap_ranges: None,
                    notes: None,
                    created_at: now,
                    updated_at: now,
                };
                if let Err(e) = self.upsert_dataset_completeness(&dc).await {
                    warn!(
                        error = %e,
                        target_id = %target_id,
                        dataset = %ds_name,
                        network = %network,
                        "Failed to update dataset completeness"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers for stamping dataset_version_id on Silver records
// ---------------------------------------------------------------------------

/// Trait for Silver records that have a `dataset_version_id` field.
trait HasDatasetVersionId {
    fn set_dataset_version_id(&mut self, id: Option<Uuid>);
}

impl HasDatasetVersionId for spectraplex_core::materializer::TokenTransfer {
    fn set_dataset_version_id(&mut self, id: Option<Uuid>) {
        self.dataset_version_id = id;
    }
}

impl HasDatasetVersionId for spectraplex_core::materializer::NativeBalanceDelta {
    fn set_dataset_version_id(&mut self, id: Option<Uuid>) {
        self.dataset_version_id = id;
    }
}

impl HasDatasetVersionId for spectraplex_core::materializer::DecodedEvent {
    fn set_dataset_version_id(&mut self, id: Option<Uuid>) {
        self.dataset_version_id = id;
    }
}

impl HasDatasetVersionId for spectraplex_core::materializer::HlFillRecord {
    fn set_dataset_version_id(&mut self, id: Option<Uuid>) {
        self.dataset_version_id = id;
    }
}

impl HasDatasetVersionId for spectraplex_core::materializer::HlFundingPayment {
    fn set_dataset_version_id(&mut self, id: Option<Uuid>) {
        self.dataset_version_id = id;
    }
}

impl HasDatasetVersionId for spectraplex_core::materializer::HlPositionChange {
    fn set_dataset_version_id(&mut self, id: Option<Uuid>) {
        self.dataset_version_id = id;
    }
}

fn stamp_dataset_version_id<T: HasDatasetVersionId>(
    records: &mut [T],
    versions: &HashMap<String, Uuid>,
    dataset_name: &str,
) {
    if let Some(&version_id) = versions.get(dataset_name) {
        for r in records.iter_mut() {
            r.set_dataset_version_id(Some(version_id));
        }
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
        let v2 = v1_tx_to_v2_raw(&tx, None);

        // Verify user_id and wallet_address are not present in serialized form
        let json = serde_json::to_string(&v2).unwrap();
        assert!(!json.contains("user_id"));
        assert!(!json.contains("wallet_address"));
    }

    #[test]
    fn v1_to_v2_solana_maps_network() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2.network, "solana-mainnet");
        assert_eq!(v2.source, "rpc");
    }

    #[test]
    fn v1_to_v2_solana_extracts_block_number_from_slot() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2.block_number, Some(298412345));
    }

    #[test]
    fn v1_to_v2_ethereum_maps_network() {
        let tx = make_eth_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2.network, "ethereum-mainnet");
        assert_eq!(v2.source, "rpc");
    }

    #[test]
    fn v1_to_v2_ethereum_extracts_block_number() {
        let tx = make_eth_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2.block_number, Some(18000000));
    }

    #[test]
    fn v1_to_v2_hyperliquid_maps_network() {
        let tx = make_hl_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2.network, "hypercore-mainnet");
        assert_eq!(v2.source, "rest");
    }

    #[test]
    fn v1_to_v2_hyperliquid_no_block_number() {
        let tx = make_hl_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2.block_number, None);
    }

    #[test]
    fn v1_to_v2_preserves_id_hash_timestamp_metadata() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2.id, tx.id);
        assert_eq!(v2.tx_hash, tx.tx_hash);
        assert_eq!(v2.timestamp, tx.timestamp);
        assert_eq!(v2.raw_metadata, tx.raw_metadata);
    }

    #[test]
    fn v1_to_v2_ingestion_run_id_is_none() {
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
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

        let v2_txs: Vec<RawTransaction> = txs.iter().map(|tx| v1_tx_to_v2_raw(tx, None)).collect();
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

        let v2_txs: Vec<RawTransaction> = txs.iter().map(|tx| v1_tx_to_v2_raw(tx, None)).collect();
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

    // -- Silver materialization extraction tests --

    #[test]
    fn silver_extraction_solana_produces_records() {
        use crate::solana_parser::{
            extract_solana_decoded_events, extract_solana_native_balance_deltas,
            extract_solana_token_transfers,
        };

        // Minimal Solana tx metadata with token balances for extraction
        let metadata = serde_json::json!({
            "transaction": {
                "signatures": ["sig1"],
                "message": {
                    "accountKeys": ["wallet1", "wallet2"],
                    "instructions": [],
                    "recentBlockhash": "hash1",
                    "header": {
                        "numRequiredSignatures": 1,
                        "numReadonlySignedAccounts": 0,
                        "numReadonlyUnsignedAccounts": 0
                    }
                }
            },
            "meta": {
                "err": null,
                "fee": 5000,
                "preBalances": [1000000, 500000],
                "postBalances": [995000, 505000],
                "preTokenBalances": [],
                "postTokenBalances": [],
                "logMessages": ["Program log: test"]
            },
            "slot": 100,
            "blockTime": 1700000000
        });

        // These may or may not produce records depending on the data,
        // but they should not panic
        let _transfers = extract_solana_token_transfers(None, "solana-mainnet", &metadata);
        let _deltas = extract_solana_native_balance_deltas(None, "solana-mainnet", &metadata);
        let _events = extract_solana_decoded_events(None, "solana-mainnet", &metadata);
    }

    #[test]
    fn silver_extraction_hyperliquid_fill_produces_records() {
        use crate::hyperliquid_parser::extract_hl_fill_records;

        // The extraction function expects {"type": "fill", "data": { ... }}
        let metadata = serde_json::json!({
            "type": "fill",
            "data": {
                "coin": "ETH",
                "px": "2000.0",
                "sz": "1.5",
                "side": "B",
                "time": 1700000000000_i64,
                "startPosition": "0.0",
                "dir": "Open Long",
                "closedPnl": "0.0",
                "hash": "0xfillhash",
                "oid": 12345,
                "crossed": false,
                "fee": "0.5",
                "tid": 99999
            }
        });

        let fills = extract_hl_fill_records(None, "hypercore-mainnet", &metadata);
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].coin, "ETH");
        assert_eq!(fills[0].network, "hypercore-mainnet");
    }

    #[test]
    fn silver_extraction_chain_to_network_mapping() {
        // Verify that chain_to_default_network produces the correct network
        // identifiers used by extraction functions
        assert_eq!(chain_to_default_network(&Chain::Solana), "solana-mainnet");
        assert_eq!(
            chain_to_default_network(&Chain::Ethereum),
            "ethereum-mainnet"
        );
        assert_eq!(
            chain_to_default_network(&Chain::Hyperliquid),
            "hypercore-mainnet"
        );
    }

    // -- stamp_dataset_version_id tests --

    #[test]
    fn stamp_dataset_version_id_sets_version_on_records() {
        use bigdecimal::BigDecimal;
        use spectraplex_core::materializer::TokenTransfer;

        let version_id = Uuid::new_v4();
        let mut versions = std::collections::HashMap::new();
        versions.insert("token_transfers".to_string(), version_id);

        let mut records = vec![TokenTransfer {
            id: Uuid::new_v4(),
            raw_transaction_id: None,
            network: "solana-mainnet".to_string(),
            token_address: "mint1".to_string(),
            token_symbol: None,
            from_address: "addr1".to_string(),
            to_address: "addr2".to_string(),
            amount: BigDecimal::from(100),
            decimals: 9,
            transfer_index: 0,
            dataset_version_id: None,
            created_at: Utc::now(),
        }];

        stamp_dataset_version_id(&mut records, &versions, "token_transfers");
        assert_eq!(records[0].dataset_version_id, Some(version_id));
    }

    #[test]
    fn stamp_dataset_version_id_noop_when_no_version() {
        use bigdecimal::BigDecimal;
        use spectraplex_core::materializer::TokenTransfer;

        let versions = std::collections::HashMap::new();

        let mut records = vec![TokenTransfer {
            id: Uuid::new_v4(),
            raw_transaction_id: None,
            network: "solana-mainnet".to_string(),
            token_address: "mint1".to_string(),
            token_symbol: None,
            from_address: "addr1".to_string(),
            to_address: "addr2".to_string(),
            amount: BigDecimal::from(100),
            decimals: 9,
            transfer_index: 0,
            dataset_version_id: None,
            created_at: Utc::now(),
        }];

        stamp_dataset_version_id(&mut records, &versions, "token_transfers");
        assert_eq!(records[0].dataset_version_id, None);
    }

    // -- raw_tx_id resolution mapping test --

    #[test]
    fn raw_tx_id_lookup_pairs_are_unique() {
        // Verify that the unique (network, tx_hash) pairs are correctly
        // derived from transactions with potential duplicates.
        let txs = [
            Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                wallet_address: "wallet1".to_string(),
                timestamp: 1700000000,
                tx_hash: "0xhash1".to_string(),
                chain: Chain::Ethereum,
                raw_metadata: serde_json::json!({"block_number": 18000000}),
            },
            Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                wallet_address: "wallet2".to_string(),
                timestamp: 1700000000,
                tx_hash: "0xhash1".to_string(), // duplicate tx_hash
                chain: Chain::Ethereum,
                raw_metadata: serde_json::json!({"block_number": 18000000}),
            },
            Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                wallet_address: "wallet1".to_string(),
                timestamp: 1700000001,
                tx_hash: "0xhash2".to_string(),
                chain: Chain::Ethereum,
                raw_metadata: serde_json::json!({"block_number": 18000001}),
            },
        ];

        let pairs: std::collections::HashSet<(String, String)> = txs
            .iter()
            .map(|tx| {
                (
                    chain_to_default_network(&tx.chain).to_string(),
                    tx.tx_hash.clone(),
                )
            })
            .collect();

        // 3 transactions but only 2 unique (network, tx_hash) pairs
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&("ethereum-mainnet".to_string(), "0xhash1".to_string())));
        assert!(pairs.contains(&("ethereum-mainnet".to_string(), "0xhash2".to_string())));
    }

    // -- Multi-wallet target match assembly --

    #[test]
    fn multi_wallet_v2_target_matches_share_raw_tx() {
        // Two wallets see the same tx_hash on the same chain. The V2 raw
        // transaction should be deduplicated, and each wallet's target should
        // get its own target_match pointing to the same raw_transaction_id.
        let shared_tx_hash = "0xshared_hash";
        let shared_chain = Chain::Ethereum;
        let shared_meta = serde_json::json!({"block_number": 18000000});

        let wallet_a_tx = Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet_a".to_string(),
            timestamp: 1700000000,
            tx_hash: shared_tx_hash.to_string(),
            chain: shared_chain.clone(),
            raw_metadata: shared_meta.clone(),
        };

        let wallet_b_tx = Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            wallet_address: "wallet_b".to_string(),
            timestamp: 1700000000,
            tx_hash: shared_tx_hash.to_string(),
            chain: shared_chain,
            raw_metadata: shared_meta,
        };

        // Both convert to V2 raw transactions with the same tx_hash
        let v2_a = v1_tx_to_v2_raw(&wallet_a_tx, None);
        let v2_b = v1_tx_to_v2_raw(&wallet_b_tx, None);
        assert_eq!(v2_a.tx_hash, v2_b.tx_hash);
        assert_eq!(v2_a.network, v2_b.network);

        // After upsert, the canonical ID would be the same for both.
        // Simulate by using a single canonical ID.
        let canonical_id = Uuid::new_v4();

        let target_a_id = Uuid::new_v4();
        let target_b_id = Uuid::new_v4();

        let matches_a = build_target_matches(target_a_id, &[canonical_id]);
        let matches_b = build_target_matches(target_b_id, &[canonical_id]);

        assert_eq!(matches_a.len(), 1);
        assert_eq!(matches_b.len(), 1);
        assert_eq!(matches_a[0].raw_transaction_id, canonical_id);
        assert_eq!(matches_b[0].raw_transaction_id, canonical_id);
        assert_eq!(matches_a[0].target_id, target_a_id);
        assert_eq!(matches_b[0].target_id, target_b_id);
        // Each match has its own unique id
        assert_ne!(matches_a[0].id, matches_b[0].id);
    }

    // -- EVM multi-log dedup within a single batch --

    #[test]
    fn evm_multi_log_dedup_produces_unique_raw_txs() {
        // An EVM transaction with 3 matching logs produces 3 V1 rows that
        // share the same tx_hash. After conversion to V2, dedup by
        // (network, tx_hash) should yield exactly 1 unique raw transaction.
        let shared_hash = "0xmultilog";
        let txs: Vec<Transaction> = (0..3)
            .map(|i| Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                wallet_address: "0xwallet".to_string(),
                timestamp: 1700000000,
                tx_hash: shared_hash.to_string(),
                chain: Chain::Ethereum,
                raw_metadata: serde_json::json!({"block_number": 18000000, "log_index": i}),
            })
            .collect();

        let v2_txs: Vec<RawTransaction> = txs.iter().map(|tx| v1_tx_to_v2_raw(tx, None)).collect();
        assert_eq!(v2_txs.len(), 3, "all 3 V1 rows convert to V2");

        // Deduplicate by (network, tx_hash)
        let mut seen = std::collections::HashMap::new();
        let mut deduped = Vec::new();
        for tx in &v2_txs {
            let key = (tx.network.as_str(), tx.tx_hash.as_str());
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(key) {
                e.insert(deduped.len());
                deduped.push(tx);
            }
        }
        assert_eq!(deduped.len(), 1, "dedup yields exactly 1 unique raw tx");
        assert_eq!(deduped[0].tx_hash, shared_hash);

        // Simulate canonical ID assignment after upsert
        let canonical_id = Uuid::new_v4();
        let id_map: std::collections::HashMap<(&str, &str), Uuid> = deduped
            .iter()
            .map(|tx| ((tx.network.as_str(), tx.tx_hash.as_str()), canonical_id))
            .collect();

        // All 3 original V2 txs should resolve to the same canonical ID
        let all_ids: Vec<Uuid> = v2_txs
            .iter()
            .filter_map(|tx| {
                id_map
                    .get(&(tx.network.as_str(), tx.tx_hash.as_str()))
                    .copied()
            })
            .collect();
        assert_eq!(all_ids.len(), 3, "all original txs get a canonical ID");
        assert!(
            all_ids.iter().all(|&id| id == canonical_id),
            "all map to the same canonical ID"
        );

        // Target matches: one per original tx, all pointing to canonical_id
        let target_id = Uuid::new_v4();
        let matches = build_target_matches(target_id, &all_ids);
        assert_eq!(matches.len(), 3, "one target_match per original V1 row");
        for m in &matches {
            assert_eq!(m.raw_transaction_id, canonical_id);
            assert_eq!(m.target_id, target_id);
        }
    }

    // -- v1_checkpoint_to_v2_with_network tests --

    #[test]
    fn checkpoint_v2_with_explicit_network_overrides_default() {
        let cp = IndexerCheckpoint {
            chain: Chain::Ethereum,
            wallet_address: "0xwallet".to_string(),
            last_signature: Some("0xabc".to_string()),
            last_slot: None,
            last_block: Some(12345),
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();

        // Without explicit network: uses chain default
        let v2_default = v1_checkpoint_to_v2(&cp, target_id);
        assert_eq!(v2_default.network, "ethereum-mainnet");

        // With explicit network: overrides to base-mainnet
        let v2_override = v1_checkpoint_to_v2_with_network(&cp, target_id, Some("base-mainnet"));
        assert_eq!(v2_override.network, "base-mainnet");
        assert_eq!(v2_override.source, "rpc");
        assert_eq!(v2_override.cursor["last_block"], 12345);
    }

    #[test]
    fn checkpoint_v2_with_none_network_uses_default() {
        let cp = IndexerCheckpoint {
            chain: Chain::Solana,
            wallet_address: "wallet".to_string(),
            last_signature: Some("sig".to_string()),
            last_slot: Some(100),
            last_block: None,
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();

        let v2 = v1_checkpoint_to_v2_with_network(&cp, target_id, None);
        assert_eq!(v2.network, "solana-mainnet");
    }

    #[test]
    fn checkpoint_v2_arbitrum_network_distinct_from_ethereum() {
        let cp = IndexerCheckpoint {
            chain: Chain::Ethereum,
            wallet_address: "0xwallet".to_string(),
            last_signature: Some("0xabc".to_string()),
            last_slot: None,
            last_block: Some(50000),
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();

        let v2_arb = v1_checkpoint_to_v2_with_network(&cp, target_id, Some("arbitrum-mainnet"));
        let v2_eth = v1_checkpoint_to_v2_with_network(&cp, target_id, None);

        assert_eq!(v2_arb.network, "arbitrum-mainnet");
        assert_eq!(v2_eth.network, "ethereum-mainnet");
        assert_ne!(v2_arb.network, v2_eth.network);
    }

    // -- v1_tx_to_v2_raw with explicit network --

    #[test]
    fn v1_tx_to_v2_raw_explicit_network_overrides_chain_default() {
        let tx = make_eth_tx();
        let v2 = v1_tx_to_v2_raw(&tx, Some("base-mainnet"));
        assert_eq!(v2.network, "base-mainnet");
        // Source is still derived from chain
        assert_eq!(v2.source, "rpc");
        // Other fields preserved
        assert_eq!(v2.tx_hash, tx.tx_hash);
        assert_eq!(v2.timestamp, tx.timestamp);
    }

    #[test]
    fn v1_tx_to_v2_raw_none_network_uses_chain_default() {
        let tx = make_eth_tx();
        let v2 = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2.network, "ethereum-mainnet");
    }

    #[test]
    fn v1_tx_to_v2_raw_base_mainnet_distinct_from_ethereum() {
        let tx = make_eth_tx();
        let v2_base = v1_tx_to_v2_raw(&tx, Some("base-mainnet"));
        let v2_eth = v1_tx_to_v2_raw(&tx, None);
        assert_eq!(v2_base.network, "base-mainnet");
        assert_eq!(v2_eth.network, "ethereum-mainnet");
        assert_ne!(v2_base.network, v2_eth.network);
    }

    #[test]
    fn v1_tx_to_v2_raw_arbitrum_network() {
        let tx = make_eth_tx();
        let v2 = v1_tx_to_v2_raw(&tx, Some("arbitrum-mainnet"));
        assert_eq!(v2.network, "arbitrum-mainnet");
        assert_eq!(v2.block_number, Some(18000000));
    }

    #[test]
    fn v1_tx_to_v2_raw_explicit_network_solana_noop() {
        // For Solana, explicit network should still work
        let tx = make_solana_tx();
        let v2 = v1_tx_to_v2_raw(&tx, Some("solana-mainnet"));
        assert_eq!(v2.network, "solana-mainnet");
    }

    // -- v2_checkpoint_to_v1 conversion --

    #[test]
    fn v2_checkpoint_to_v1_solana_roundtrip() {
        let original = IndexerCheckpoint {
            chain: Chain::Solana,
            wallet_address: "wallet123".to_string(),
            last_signature: Some("5VERv8NMhzg".to_string()),
            last_slot: Some(298412345),
            last_block: None,
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();
        let v2 = v1_checkpoint_to_v2(&original, target_id);

        // Convert back to V1
        let v1 = v2_checkpoint_to_v1(&v2, &Chain::Solana, "wallet123");
        assert!(matches!(v1.chain, Chain::Solana));
        assert_eq!(v1.wallet_address, "wallet123");
        assert_eq!(v1.last_signature, Some("5VERv8NMhzg".to_string()));
        assert_eq!(v1.last_slot, Some(298412345));
        assert_eq!(v1.last_block, None);
        // Solana cursor does not store last_timestamp
        assert_eq!(v1.last_timestamp, None);
    }

    #[test]
    fn v2_checkpoint_to_v1_ethereum_roundtrip() {
        let original = IndexerCheckpoint {
            chain: Chain::Ethereum,
            wallet_address: "0xwallet".to_string(),
            last_signature: Some("0xabc".to_string()),
            last_slot: None,
            last_block: Some(21500000),
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();
        let v2 = v1_checkpoint_to_v2(&original, target_id);

        let v1 = v2_checkpoint_to_v1(&v2, &Chain::Ethereum, "0xwallet");
        assert!(matches!(v1.chain, Chain::Ethereum));
        assert_eq!(v1.wallet_address, "0xwallet");
        // EVM cursor stores only last_block
        assert_eq!(v1.last_block, Some(21500000));
        assert_eq!(v1.last_slot, None);
        assert_eq!(v1.last_signature, None);
    }

    #[test]
    fn v2_checkpoint_to_v1_hyperliquid_roundtrip() {
        let original = IndexerCheckpoint {
            chain: Chain::Hyperliquid,
            wallet_address: "0xhl".to_string(),
            last_signature: None,
            last_slot: None,
            last_block: None,
            last_timestamp: Some(1700000000),
        };
        let target_id = Uuid::new_v4();
        let v2 = v1_checkpoint_to_v2(&original, target_id);

        let v1 = v2_checkpoint_to_v1(&v2, &Chain::Hyperliquid, "0xhl");
        assert!(matches!(v1.chain, Chain::Hyperliquid));
        assert_eq!(v1.wallet_address, "0xhl");
        assert_eq!(v1.last_timestamp, Some(1700000000));
    }

    #[test]
    fn v2_checkpoint_to_v1_empty_cursor() {
        let v2 = Checkpoint {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            network: "ethereum-mainnet".to_string(),
            source: "rpc".to_string(),
            cursor: serde_json::json!({}),
            updated_at: Utc::now(),
        };

        let v1 = v2_checkpoint_to_v1(&v2, &Chain::Ethereum, "0xwallet");
        assert_eq!(v1.last_signature, None);
        assert_eq!(v1.last_slot, None);
        assert_eq!(v1.last_block, None);
        assert_eq!(v1.last_timestamp, None);
    }

    #[test]
    fn v2_checkpoint_to_v1_preserves_wallet_and_chain() {
        let v2 = Checkpoint {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            network: "base-mainnet".to_string(),
            source: "rpc".to_string(),
            cursor: serde_json::json!({"last_block": 50000}),
            updated_at: Utc::now(),
        };

        let v1 = v2_checkpoint_to_v1(&v2, &Chain::Ethereum, "0xbase_wallet");
        assert!(matches!(v1.chain, Chain::Ethereum));
        assert_eq!(v1.wallet_address, "0xbase_wallet");
        assert_eq!(v1.last_block, Some(50000));
    }

    // -- Batch conversion with explicit network --

    #[test]
    fn batch_conversion_with_explicit_network_preserves_network() {
        let txs: Vec<Transaction> = (0..3)
            .map(|i| Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                wallet_address: "0xwallet".to_string(),
                timestamp: 1700000000 + i,
                tx_hash: format!("0xhash{i}"),
                chain: Chain::Ethereum,
                raw_metadata: serde_json::json!({"block_number": 18000000 + i}),
            })
            .collect();

        let v2_txs: Vec<RawTransaction> = txs
            .iter()
            .map(|tx| v1_tx_to_v2_raw(tx, Some("base-mainnet")))
            .collect();

        assert_eq!(v2_txs.len(), 3);
        for v2 in &v2_txs {
            assert_eq!(v2.network, "base-mainnet");
        }
    }

    #[test]
    fn dual_write_batch_assembly_with_explicit_network() {
        let target_id = Uuid::new_v4();
        let txs: Vec<Transaction> = (0..3)
            .map(|i| Transaction {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                wallet_address: "0xwallet".to_string(),
                timestamp: 1700000000 + i,
                tx_hash: format!("0xhash{i}"),
                chain: Chain::Ethereum,
                raw_metadata: serde_json::json!({"block_number": 18000000 + i}),
            })
            .collect();

        let v2_txs: Vec<RawTransaction> = txs
            .iter()
            .map(|tx| v1_tx_to_v2_raw(tx, Some("arbitrum-mainnet")))
            .collect();
        let raw_ids: Vec<Uuid> = v2_txs.iter().map(|rt| rt.id).collect();
        let matches = build_target_matches(target_id, &raw_ids);

        assert_eq!(v2_txs.len(), 3);
        assert_eq!(matches.len(), 3);
        for v2_tx in &v2_txs {
            assert_eq!(v2_tx.network, "arbitrum-mainnet");
            assert_eq!(v2_tx.source, "rpc");
        }
    }

    // -- resolve_effective_network --

    #[test]
    fn resolve_network_explicit_wins_over_bronze_and_chain_default() {
        let tx = make_eth_tx();
        let mut bronze_map = HashMap::new();
        bronze_map.insert(tx.tx_hash.clone(), vec!["base-mainnet".to_string()]);

        let (network, inferred) =
            resolve_effective_network(&tx, Some("arbitrum-mainnet"), &bronze_map);
        assert_eq!(network, "arbitrum-mainnet");
        assert!(
            !inferred,
            "explicit_network should not be marked as inferred"
        );
    }

    #[test]
    fn resolve_network_bronze_single_match_wins_over_chain_default() {
        let tx = make_eth_tx();
        let mut bronze_map = HashMap::new();
        bronze_map.insert(tx.tx_hash.clone(), vec!["base-mainnet".to_string()]);

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert_eq!(network, "base-mainnet");
        assert!(!inferred);
    }

    #[test]
    fn resolve_network_falls_back_to_chain_default_when_no_bronze_row() {
        let tx = make_eth_tx();
        let bronze_map = HashMap::new();

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert_eq!(network, "ethereum-mainnet");
        assert!(inferred, "chain default should be marked as inferred");
    }

    #[test]
    fn resolve_network_solana_from_bronze() {
        let tx = make_solana_tx();
        let mut bronze_map = HashMap::new();
        bronze_map.insert(tx.tx_hash.clone(), vec!["solana-mainnet".to_string()]);

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert_eq!(network, "solana-mainnet");
        assert!(!inferred);
    }

    #[test]
    fn resolve_network_hyperliquid_chain_default() {
        let tx = make_hl_tx();
        let bronze_map = HashMap::new();

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert_eq!(network, "hypercore-mainnet");
        assert!(inferred);
    }

    #[test]
    fn resolve_network_evm_l2_not_collapsed_to_ethereum() {
        let tx = make_eth_tx();
        let mut bronze_map = HashMap::new();
        bronze_map.insert(tx.tx_hash.clone(), vec!["base-mainnet".to_string()]);

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert_eq!(network, "base-mainnet");
        assert!(!inferred);
        assert_ne!(
            network,
            chain_to_default_network(&tx.chain),
            "should NOT collapse to ethereum-mainnet"
        );
    }

    #[test]
    fn resolve_network_explicit_none_bronze_none_gives_chain_default_with_flag() {
        let tx = make_eth_tx();
        let bronze_map = HashMap::new();

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert_eq!(network, "ethereum-mainnet");
        assert!(
            inferred,
            "must flag as inferred so callers can log a warning"
        );
    }

    #[test]
    fn resolve_network_cross_family_disambiguates_correctly() {
        // Same tx_hash exists on both ethereum-mainnet and solana-mainnet.
        // A Chain::Ethereum tx should resolve to ethereum-mainnet (single
        // family match → authoritative).
        let tx = make_eth_tx();
        let mut bronze_map = HashMap::new();
        bronze_map.insert(
            tx.tx_hash.clone(),
            vec!["solana-mainnet".to_string(), "ethereum-mainnet".to_string()],
        );

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert_eq!(network, "ethereum-mainnet");
        assert!(!inferred, "single family match is authoritative");
    }

    #[test]
    fn resolve_network_same_family_ambiguity_is_inferred() {
        // Same tx_hash on ethereum-mainnet and base-mainnet (both EVM).
        // This is ambiguous — should be marked as inferred so callers
        // can fail closed instead of silently guessing.
        let tx = make_eth_tx();
        let mut bronze_map = HashMap::new();
        bronze_map.insert(
            tx.tx_hash.clone(),
            vec!["base-mainnet".to_string(), "ethereum-mainnet".to_string()],
        );

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert!(
            inferred,
            "same-family ambiguity must be flagged as inferred"
        );
        // Still returns a deterministic pick (chain default preferred)
        assert_eq!(network, "ethereum-mainnet");
    }

    #[test]
    fn resolve_network_same_family_no_default_is_inferred() {
        // Same tx_hash on base-mainnet and arbitrum-mainnet (both EVM, no
        // ethereum-mainnet default). Ambiguous — must be inferred.
        let tx = make_eth_tx();
        let mut bronze_map = HashMap::new();
        bronze_map.insert(
            tx.tx_hash.clone(),
            vec!["base-mainnet".to_string(), "arbitrum-mainnet".to_string()],
        );

        let (network, inferred) = resolve_effective_network(&tx, None, &bronze_map);
        assert!(
            inferred,
            "same-family ambiguity must be flagged as inferred"
        );
        // Deterministic: alphabetical min
        assert_eq!(network, "arbitrum-mainnet");
    }
}
