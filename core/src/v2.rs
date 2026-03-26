use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};
use uuid::Uuid;

use crate::models::Chain;

// ---------------------------------------------------------------------------
// Chain Family
// ---------------------------------------------------------------------------

/// Coarse protocol family. Serializes to lowercase strings matching the
/// `chain_family_enum` SQL type defined in P0-W3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ChainFamily {
    Solana,
    Evm,
    Hyperliquid,
}

impl From<Chain> for ChainFamily {
    fn from(chain: Chain) -> Self {
        match chain {
            Chain::Solana => ChainFamily::Solana,
            Chain::Ethereum => ChainFamily::Evm,
            Chain::Hyperliquid => ChainFamily::Hyperliquid,
        }
    }
}

// ---------------------------------------------------------------------------
// Finality Model
// ---------------------------------------------------------------------------

/// Per-network finality semantics. Four variants matching P0-W3 Section 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum FinalityModel {
    ProbabilisticSlot,
    ProbabilisticBlock,
    DeterministicBlock,
    Instant,
}

// ---------------------------------------------------------------------------
// Target Kind
// ---------------------------------------------------------------------------

/// Eight target kinds matching P0-W2 Section 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TargetKind {
    Wallet,
    Contract,
    Program,
    Account,
    TopicFilter,
    Market,
    Pool,
    Protocol,
}

impl TargetKind {
    /// Returns `true` if this target kind is valid (or limited) for the given
    /// chain family, per the P0-W2 Section 3 validity matrix.
    ///
    /// "Limited" combinations (e.g. Account on EVM) return `true` — callers
    /// that want to warn about limited support should check separately.
    pub fn valid_for_chain_family(&self, family: ChainFamily) -> bool {
        match (self, family) {
            // wallet: valid on all three families
            (TargetKind::Wallet, _) => true,

            // contract: EVM only
            (TargetKind::Contract, ChainFamily::Evm) => true,
            (TargetKind::Contract, _) => false,

            // program: Solana only
            (TargetKind::Program, ChainFamily::Solana) => true,
            (TargetKind::Program, _) => false,

            // account: Solana (valid), EVM (limited → true), Hyperliquid (not valid)
            (TargetKind::Account, ChainFamily::Solana) => true,
            (TargetKind::Account, ChainFamily::Evm) => true,
            (TargetKind::Account, ChainFamily::Hyperliquid) => false,

            // topic_filter: EVM only
            (TargetKind::TopicFilter, ChainFamily::Evm) => true,
            (TargetKind::TopicFilter, _) => false,

            // market: Hyperliquid only
            (TargetKind::Market, ChainFamily::Hyperliquid) => true,
            (TargetKind::Market, _) => false,

            // pool: Solana and EVM
            (TargetKind::Pool, ChainFamily::Solana) => true,
            (TargetKind::Pool, ChainFamily::Evm) => true,
            (TargetKind::Pool, ChainFamily::Hyperliquid) => false,

            // protocol: Solana and EVM
            (TargetKind::Protocol, ChainFamily::Solana) => true,
            (TargetKind::Protocol, ChainFamily::Evm) => true,
            (TargetKind::Protocol, ChainFamily::Hyperliquid) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Target Mode
// ---------------------------------------------------------------------------

/// Controls which connector methods are invoked. Matches P0-W2 Section 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TargetMode {
    Backfill,
    Stream,
    Both,
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

/// Protocol-level facts about a specific chain network.
/// Matches P0-W3 Section 1.1 / Section 3.1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Network {
    pub id: String,
    pub chain_family: ChainFamily,
    pub display_name: String,
    pub is_testnet: bool,
    pub finality_model: FinalityModel,
    /// Typical block/slot time in milliseconds. `None` for blockless chains
    /// (e.g. HyperCore).
    pub block_time_ms: Option<i32>,
}

// ---------------------------------------------------------------------------
// Index Target
// ---------------------------------------------------------------------------

/// A registered indexing subject. Matches P0-W1 Section 3.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexTarget {
    pub id: Uuid,
    pub kind: TargetKind,
    pub network: String,
    pub chain_family: ChainFamily,
    pub address: Option<String>,
    pub filter_spec: Option<serde_json::Value>,
    pub mode: TargetMode,
    pub label: Option<String>,
    pub owner_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Raw Transaction (Bronze)
// ---------------------------------------------------------------------------

/// Canonical raw transaction record — target-agnostic.
/// No `user_id`, no `wallet_address` per P0-W1 Section 3.1 rules 1-2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTransaction {
    pub id: Uuid,
    pub network: String,
    pub tx_hash: String,
    pub timestamp: i64,
    pub block_number: Option<i64>,
    pub raw_metadata: serde_json::Value,
    pub source: String,
    pub ingestion_run_id: Option<Uuid>,
    pub ingested_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Raw EVM Trace (Bronze)
// ---------------------------------------------------------------------------

/// Supported tracer types for `debug_traceTransaction`.
///
/// Each variant corresponds to a built-in Geth tracer. The `Other` variant
/// accommodates node-specific or future tracers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "camelCase")]
#[strum(serialize_all = "camelCase")]
pub enum EvmTraceType {
    /// Geth `callTracer` — nested call tree with value transfers.
    CallTracer,
    /// Geth `prestateTracer` — pre/post state diffs per account.
    PrestateTracer,
    /// Geth `flatCallTracer` — flat list of calls (Erigon/Reth).
    FlatCallTracer,
    /// Any other tracer type.
    Other,
}

/// Bronze-layer raw EVM trace record.
///
/// Stores the full JSON response from `debug_traceTransaction` (or
/// `trace_transaction`) for a single transaction. The `raw_trace` field
/// contains the complete tracer output as JSONB, preserving all data for
/// downstream Silver-layer parsing (e.g. NativeBalanceDelta extraction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEvmTrace {
    pub id: Uuid,
    pub transaction_hash: String,
    pub block_number: Option<i64>,
    pub network: String,
    pub trace_type: EvmTraceType,
    pub raw_trace: serde_json::Value,
    pub ingestion_run_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Target Match
// ---------------------------------------------------------------------------

/// Join record linking a raw transaction to an index target.
/// Matches P0-W1 Section 3.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetMatch {
    pub id: Uuid,
    pub target_id: Uuid,
    pub raw_transaction_id: Uuid,
    pub match_reason: Option<String>,
    pub matched_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Ingestion Run
// ---------------------------------------------------------------------------

/// Control-plane record for a single ingestion operation.
/// Matches P0-W1 Section 3.4.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionRun {
    pub id: Uuid,
    pub target_id: Option<Uuid>,
    pub network: String,
    pub source: String,
    pub mode: String,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub records_written: i64,
    pub error_message: Option<String>,
    pub cursor_state: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

/// V2 checkpoint keyed on (target_id, network, source).
/// Matches P0-W1 Section 3.4 / P0-W3 Section 5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: Uuid,
    pub target_id: Uuid,
    pub network: String,
    pub source: String,
    pub cursor: serde_json::Value,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Dataset Version Status
// ---------------------------------------------------------------------------

/// Status of a dataset version in the registry lifecycle.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DatasetVersionStatus {
    /// Currently active version used for new materializations.
    #[default]
    Active,
    /// Replaced by a newer version; retained for historical queries.
    Superseded,
    /// Version failed during materialization and should not be used.
    Failed,
}

// ---------------------------------------------------------------------------
// Dataset Version
// ---------------------------------------------------------------------------

/// Tracks parser/materializer versions for safe reprocessing.
/// Matches P0-W1 Section 3.5, extended in P3-W1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetVersion {
    pub id: Uuid,
    pub dataset_name: String,
    pub version: i32,
    pub parser_hash: Option<String>,
    pub created_at: DateTime<Utc>,
    pub notes: Option<String>,
    /// Lifecycle status of this version.
    #[serde(default)]
    pub status: DatasetVersionStatus,
}

// ---------------------------------------------------------------------------
// Ingestion Batch
// ---------------------------------------------------------------------------

/// Output of a connector backfill call.
/// Matches P0-W1 Section 4.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionBatch {
    pub records: Vec<RawTransaction>,
    pub checkpoint: Option<Checkpoint>,
    pub run_metadata: Option<IngestionRun>,
}

// ---------------------------------------------------------------------------
// Completeness Status
// ---------------------------------------------------------------------------

/// Tracks the completeness state of a dataset for a given target and network.
/// Serializes to lowercase strings matching the SQL CHECK constraint.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Display, EnumString,
)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum CompletenessStatus {
    /// Dataset coverage is incomplete — some data is present but gaps exist.
    #[default]
    Partial,
    /// Dataset is fully covered for the target and time range.
    Complete,
    /// A backfill operation is actively filling gaps.
    Backfilling,
    /// Known gap(s) exist in coverage that have not yet been addressed.
    Gap,
}

// ---------------------------------------------------------------------------
// Dataset Completeness
// ---------------------------------------------------------------------------

/// Tracks dataset completeness per target and time range.
///
/// Downstream consumers use this to determine whether a dataset is partial,
/// complete, or backfilling for a given target and network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetCompleteness {
    pub id: Uuid,
    /// FK to index_targets.
    pub target_id: Uuid,
    /// Canonical dataset name (e.g. "token_transfers", "ledger_entries").
    pub dataset_name: String,
    /// Optional FK to dataset_versions — links to the specific version.
    pub dataset_version_id: Option<Uuid>,
    /// Network identifier (FK to networks).
    pub network: String,
    /// Current completeness status.
    pub status: CompletenessStatus,
    /// Start of the covered time range (Unix timestamp). `None` if unknown.
    pub coverage_start: Option<i64>,
    /// End of the covered time range (Unix timestamp). `None` if unknown.
    pub coverage_end: Option<i64>,
    /// Start of the covered block range. `None` for blockless chains.
    pub block_start: Option<i64>,
    /// End of the covered block range. `None` for blockless chains.
    pub block_end: Option<i64>,
    /// FK to the most recent ingestion run that updated this record.
    pub last_ingestion_run_id: Option<Uuid>,
    /// Number of records in the dataset for this target + network.
    pub records_count: i64,
    /// JSON array of gap ranges, e.g. `[{"start": 100, "end": 200}]`.
    pub gap_ranges: Option<serde_json::Value>,
    /// Optional human-readable notes.
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Ingestion Job (Durable Control Plane)
// ---------------------------------------------------------------------------

/// Status of an ingestion job in the durable queue.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum IngestionJobStatus {
    #[default]
    Pending,
    Claimed,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Ingestion mode for a queued job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum IngestionJobMode {
    Backfill,
    Incremental,
}

/// A queued ingestion work item stored in Postgres.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionJob {
    pub id: Uuid,
    pub target_id: Option<Uuid>,
    pub network: String,
    pub mode: IngestionJobMode,
    pub status: IngestionJobStatus,
    pub priority: i32,
    pub idempotency_key: Option<String>,
    pub requested_by: Option<String>,
    pub callback_url: Option<String>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A single worker attempt on an ingestion job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestionJobAttempt {
    pub id: Uuid,
    pub job_id: Uuid,
    pub worker_id: String,
    pub attempt_num: i32,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub heartbeat_at: DateTime<Utc>,
    pub error_message: Option<String>,
}

// ---------------------------------------------------------------------------
// Stream Subscription (Durable Control Plane)
// ---------------------------------------------------------------------------

/// Desired lifecycle state for a stream subscription.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StreamDesiredStatus {
    #[default]
    Active,
    Paused,
    Stopped,
}

/// Actual runtime state of a stream subscription.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StreamActualStatus {
    #[default]
    Pending,
    Running,
    Paused,
    Stopped,
    Error,
}

/// Stream source type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StreamSource {
    Grpc,
    Ws,
    Rpc,
}

/// A durable stream subscription stored in Postgres.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamSubscription {
    pub id: Uuid,
    pub target_id: Option<Uuid>,
    pub network: String,
    pub source: StreamSource,
    pub desired_status: StreamDesiredStatus,
    pub actual_status: StreamActualStatus,
    pub lease_owner: Option<String>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub cursor_state: Option<serde_json::Value>,
    pub config: Option<serde_json::Value>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Export Job (Durable Control Plane)
// ---------------------------------------------------------------------------

/// Status of an export job.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ExportJobStatus {
    #[default]
    Pending,
    Running,
    Delivering,
    Completed,
    Failed,
}

/// A durable export job stored in Postgres.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportJob {
    pub id: Uuid,
    pub dataset: String,
    pub format: String,
    pub filters: Option<serde_json::Value>,
    pub sink_config: Option<serde_json::Value>,
    pub status: ExportJobStatus,
    pub worker_id: Option<String>,
    pub record_count: Option<i32>,
    pub result_location: Option<String>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Materialization Run (Durable Control Plane)
// ---------------------------------------------------------------------------

/// Status of a materialization run.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, Display, EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum MaterializationRunStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Failed,
}

/// A durable materialization run tracking ETL provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializationRun {
    pub id: Uuid,
    pub dataset_name: String,
    pub scope: Option<serde_json::Value>,
    pub input_watermark: Option<i64>,
    pub output_record_count: i64,
    pub status: MaterializationRunStatus,
    pub dataset_version_id: Option<Uuid>,
    pub worker_id: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Address normalization helpers
// ---------------------------------------------------------------------------

/// Normalize an EVM (or Hyperliquid) address to lowercase hex.
/// Per P0-W2 Section 6.4: storage and comparison use lowercase.
pub fn normalize_evm_address(addr: &str) -> String {
    addr.to_lowercase()
}

/// Normalize a Solana address — base58 passthrough (case-sensitive by
/// definition). Per P0-W2 Section 6.4.
pub fn normalize_solana_address(addr: &str) -> String {
    addr.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // -- ChainFamily --

    #[test]
    fn chain_family_serde_roundtrip() {
        for (variant, expected_str) in [
            (ChainFamily::Solana, "\"solana\""),
            (ChainFamily::Evm, "\"evm\""),
            (ChainFamily::Hyperliquid, "\"hyperliquid\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: ChainFamily = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn chain_family_display_lowercase() {
        assert_eq!(ChainFamily::Solana.to_string(), "solana");
        assert_eq!(ChainFamily::Evm.to_string(), "evm");
        assert_eq!(ChainFamily::Hyperliquid.to_string(), "hyperliquid");
    }

    #[test]
    fn chain_family_from_str() {
        assert_eq!(
            ChainFamily::from_str("solana").unwrap(),
            ChainFamily::Solana
        );
        assert_eq!(ChainFamily::from_str("evm").unwrap(), ChainFamily::Evm);
        assert_eq!(
            ChainFamily::from_str("hyperliquid").unwrap(),
            ChainFamily::Hyperliquid
        );
        assert!(ChainFamily::from_str("bitcoin").is_err());
    }

    #[test]
    fn chain_to_chain_family_conversion() {
        assert_eq!(ChainFamily::from(Chain::Solana), ChainFamily::Solana);
        assert_eq!(ChainFamily::from(Chain::Ethereum), ChainFamily::Evm);
        assert_eq!(
            ChainFamily::from(Chain::Hyperliquid),
            ChainFamily::Hyperliquid
        );
    }

    // -- FinalityModel --

    #[test]
    fn finality_model_serde_roundtrip() {
        for (variant, expected_str) in [
            (FinalityModel::ProbabilisticSlot, "\"probabilistic-slot\""),
            (FinalityModel::ProbabilisticBlock, "\"probabilistic-block\""),
            (FinalityModel::DeterministicBlock, "\"deterministic-block\""),
            (FinalityModel::Instant, "\"instant\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: FinalityModel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn finality_model_display() {
        assert_eq!(
            FinalityModel::ProbabilisticSlot.to_string(),
            "probabilistic-slot"
        );
        assert_eq!(
            FinalityModel::ProbabilisticBlock.to_string(),
            "probabilistic-block"
        );
        assert_eq!(
            FinalityModel::DeterministicBlock.to_string(),
            "deterministic-block"
        );
        assert_eq!(FinalityModel::Instant.to_string(), "instant");
    }

    #[test]
    fn finality_model_from_str() {
        assert_eq!(
            FinalityModel::from_str("probabilistic-slot").unwrap(),
            FinalityModel::ProbabilisticSlot
        );
        assert_eq!(
            FinalityModel::from_str("deterministic-block").unwrap(),
            FinalityModel::DeterministicBlock
        );
        assert!(FinalityModel::from_str("unknown").is_err());
    }

    // -- TargetKind --

    #[test]
    fn target_kind_serde_roundtrip() {
        let variants = [
            (TargetKind::Wallet, "\"wallet\""),
            (TargetKind::Contract, "\"contract\""),
            (TargetKind::Program, "\"program\""),
            (TargetKind::Account, "\"account\""),
            (TargetKind::TopicFilter, "\"topic_filter\""),
            (TargetKind::Market, "\"market\""),
            (TargetKind::Pool, "\"pool\""),
            (TargetKind::Protocol, "\"protocol\""),
        ];
        assert_eq!(variants.len(), 8, "must have exactly 8 target kinds");
        for (variant, expected_str) in variants {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: TargetKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn target_kind_from_str() {
        assert_eq!(TargetKind::from_str("wallet").unwrap(), TargetKind::Wallet);
        assert_eq!(
            TargetKind::from_str("topic_filter").unwrap(),
            TargetKind::TopicFilter
        );
        assert!(TargetKind::from_str("invalid").is_err());
    }

    // -- TargetMode --

    #[test]
    fn target_mode_serde_roundtrip() {
        for (variant, expected_str) in [
            (TargetMode::Backfill, "\"backfill\""),
            (TargetMode::Stream, "\"stream\""),
            (TargetMode::Both, "\"both\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: TargetMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    // -- Validity matrix --

    #[test]
    fn validity_matrix_wallet_valid_everywhere() {
        assert!(TargetKind::Wallet.valid_for_chain_family(ChainFamily::Solana));
        assert!(TargetKind::Wallet.valid_for_chain_family(ChainFamily::Evm));
        assert!(TargetKind::Wallet.valid_for_chain_family(ChainFamily::Hyperliquid));
    }

    #[test]
    fn validity_matrix_contract_evm_only() {
        assert!(!TargetKind::Contract.valid_for_chain_family(ChainFamily::Solana));
        assert!(TargetKind::Contract.valid_for_chain_family(ChainFamily::Evm));
        assert!(!TargetKind::Contract.valid_for_chain_family(ChainFamily::Hyperliquid));
    }

    #[test]
    fn validity_matrix_program_solana_only() {
        assert!(TargetKind::Program.valid_for_chain_family(ChainFamily::Solana));
        assert!(!TargetKind::Program.valid_for_chain_family(ChainFamily::Evm));
        assert!(!TargetKind::Program.valid_for_chain_family(ChainFamily::Hyperliquid));
    }

    #[test]
    fn validity_matrix_account() {
        assert!(TargetKind::Account.valid_for_chain_family(ChainFamily::Solana));
        // EVM: limited → true
        assert!(TargetKind::Account.valid_for_chain_family(ChainFamily::Evm));
        assert!(!TargetKind::Account.valid_for_chain_family(ChainFamily::Hyperliquid));
    }

    #[test]
    fn validity_matrix_topic_filter_evm_only() {
        assert!(!TargetKind::TopicFilter.valid_for_chain_family(ChainFamily::Solana));
        assert!(TargetKind::TopicFilter.valid_for_chain_family(ChainFamily::Evm));
        assert!(!TargetKind::TopicFilter.valid_for_chain_family(ChainFamily::Hyperliquid));
    }

    #[test]
    fn validity_matrix_market_hyperliquid_only() {
        assert!(!TargetKind::Market.valid_for_chain_family(ChainFamily::Solana));
        assert!(!TargetKind::Market.valid_for_chain_family(ChainFamily::Evm));
        assert!(TargetKind::Market.valid_for_chain_family(ChainFamily::Hyperliquid));
    }

    #[test]
    fn validity_matrix_pool() {
        assert!(TargetKind::Pool.valid_for_chain_family(ChainFamily::Solana));
        assert!(TargetKind::Pool.valid_for_chain_family(ChainFamily::Evm));
        assert!(!TargetKind::Pool.valid_for_chain_family(ChainFamily::Hyperliquid));
    }

    #[test]
    fn validity_matrix_protocol() {
        assert!(TargetKind::Protocol.valid_for_chain_family(ChainFamily::Solana));
        assert!(TargetKind::Protocol.valid_for_chain_family(ChainFamily::Evm));
        assert!(!TargetKind::Protocol.valid_for_chain_family(ChainFamily::Hyperliquid));
    }

    // -- Address normalization --

    #[test]
    fn evm_address_normalized_to_lowercase() {
        let checksummed = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let normalized = normalize_evm_address(checksummed);
        assert_eq!(normalized, "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    }

    #[test]
    fn evm_address_already_lowercase() {
        let lower = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        assert_eq!(normalize_evm_address(lower), lower);
    }

    #[test]
    fn solana_address_preserved() {
        let addr = "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy";
        assert_eq!(normalize_solana_address(addr), addr);
    }

    // -- Struct construction --

    #[test]
    fn network_construction() {
        let net = Network {
            id: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            display_name: "Solana Mainnet".to_string(),
            is_testnet: false,
            finality_model: FinalityModel::ProbabilisticSlot,
            block_time_ms: Some(400),
        };
        let json = serde_json::to_string(&net).unwrap();
        let back: Network = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "solana-mainnet");
        assert_eq!(back.chain_family, ChainFamily::Solana);
        assert_eq!(back.finality_model, FinalityModel::ProbabilisticSlot);
        assert_eq!(back.block_time_ms, Some(400));
        assert!(!back.is_testnet);
    }

    #[test]
    fn network_blockless_chain() {
        let net = Network {
            id: "hypercore-mainnet".to_string(),
            chain_family: ChainFamily::Hyperliquid,
            display_name: "HyperCore Mainnet".to_string(),
            is_testnet: false,
            finality_model: FinalityModel::Instant,
            block_time_ms: None,
        };
        assert!(net.block_time_ms.is_none());
    }

    #[test]
    fn index_target_wallet_construction() {
        let now = Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string()),
            filter_spec: None,
            mode: TargetMode::Both,
            label: Some("My Wallet".to_string()),
            owner_id: Some(Uuid::new_v4()),
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&target).unwrap();
        let back: IndexTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, TargetKind::Wallet);
        assert_eq!(back.chain_family, ChainFamily::Solana);
        assert!(back.filter_spec.is_none());
    }

    #[test]
    fn index_target_topic_filter_construction() {
        let now = Utc::now();
        let spec = serde_json::json!({
            "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                null,
                "0x000000000000000000000000742d35cc6634c0532925a3b844bc9e7595f2bd18"
            ]
        });
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::TopicFilter,
            network: "ethereum-mainnet".to_string(),
            chain_family: ChainFamily::Evm,
            address: None,
            filter_spec: Some(spec.clone()),
            mode: TargetMode::Backfill,
            label: Some("Transfer watcher".to_string()),
            owner_id: None,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&target).unwrap();
        let back: IndexTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, TargetKind::TopicFilter);
        assert!(back.address.is_none());
        assert_eq!(back.filter_spec.unwrap(), spec);
    }

    #[test]
    fn index_target_each_kind_constructs() {
        let now = Utc::now();
        let kinds_and_families = [
            (TargetKind::Wallet, ChainFamily::Solana),
            (TargetKind::Contract, ChainFamily::Evm),
            (TargetKind::Program, ChainFamily::Solana),
            (TargetKind::Account, ChainFamily::Solana),
            (TargetKind::TopicFilter, ChainFamily::Evm),
            (TargetKind::Market, ChainFamily::Hyperliquid),
            (TargetKind::Pool, ChainFamily::Evm),
            (TargetKind::Protocol, ChainFamily::Evm),
        ];
        for (kind, family) in kinds_and_families {
            assert!(
                kind.valid_for_chain_family(family),
                "{kind:?} on {family:?}"
            );
            let target = IndexTarget {
                id: Uuid::new_v4(),
                kind,
                network: "test-net".to_string(),
                chain_family: family,
                address: Some("test-addr".to_string()),
                filter_spec: None,
                mode: TargetMode::Both,
                label: None,
                owner_id: None,
                created_at: now,
                updated_at: now,
            };
            let json = serde_json::to_string(&target).unwrap();
            let back: IndexTarget = serde_json::from_str(&json).unwrap();
            assert_eq!(back.kind, kind);
        }
    }

    #[test]
    fn raw_transaction_has_no_wallet_or_user_fields() {
        let rt = RawTransaction {
            id: Uuid::new_v4(),
            network: "solana-mainnet".to_string(),
            tx_hash: "abc123".to_string(),
            timestamp: 1700000000,
            block_number: Some(100),
            raw_metadata: serde_json::json!({"slot": 100}),
            source: "rpc".to_string(),
            ingestion_run_id: None,
            ingested_at: Utc::now(),
        };
        let json = serde_json::to_string(&rt).unwrap();
        // Verify no wallet_address or user_id in serialized output
        assert!(!json.contains("wallet_address"));
        assert!(!json.contains("user_id"));
        let back: RawTransaction = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tx_hash, "abc123");
    }

    #[test]
    fn raw_transaction_serde_roundtrip() {
        let rt = RawTransaction {
            id: Uuid::new_v4(),
            network: "ethereum-mainnet".to_string(),
            tx_hash: "0xdeadbeef".to_string(),
            timestamp: 1700000000,
            block_number: Some(18_000_000),
            raw_metadata: serde_json::json!({"gasUsed": "21000"}),
            source: "rpc".to_string(),
            ingestion_run_id: Some(Uuid::new_v4()),
            ingested_at: Utc::now(),
        };
        let json = serde_json::to_string(&rt).unwrap();
        let back: RawTransaction = serde_json::from_str(&json).unwrap();
        assert_eq!(back.network, "ethereum-mainnet");
        assert_eq!(back.block_number, Some(18_000_000));
    }

    #[test]
    fn target_match_serde_roundtrip() {
        let tm = TargetMatch {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            raw_transaction_id: Uuid::new_v4(),
            match_reason: Some("sender".to_string()),
            matched_at: Utc::now(),
        };
        let json = serde_json::to_string(&tm).unwrap();
        let back: TargetMatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.match_reason, Some("sender".to_string()));
    }

    #[test]
    fn ingestion_run_serde_roundtrip() {
        let run = IngestionRun {
            id: Uuid::new_v4(),
            target_id: Some(Uuid::new_v4()),
            network: "solana-mainnet".to_string(),
            source: "rpc".to_string(),
            mode: "backfill".to_string(),
            status: "completed".to_string(),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            records_written: 42,
            error_message: None,
            cursor_state: Some(serde_json::json!({"last_slot": 200})),
        };
        let json = serde_json::to_string(&run).unwrap();
        let back: IngestionRun = serde_json::from_str(&json).unwrap();
        assert_eq!(back.records_written, 42);
        assert_eq!(back.status, "completed");
    }

    #[test]
    fn checkpoint_serde_roundtrip() {
        let cp = Checkpoint {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            network: "solana-mainnet".to_string(),
            source: "grpc".to_string(),
            cursor: serde_json::json!({"last_signature": "abc", "last_slot": 300}),
            updated_at: Utc::now(),
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, "grpc");
        assert_eq!(back.cursor["last_slot"], 300);
    }

    #[test]
    fn dataset_version_serde_roundtrip() {
        let dv = DatasetVersion {
            id: Uuid::new_v4(),
            dataset_name: "ledger_entries".to_string(),
            version: 1,
            parser_hash: Some("sha256:abc123".to_string()),
            created_at: Utc::now(),
            notes: Some("initial release".to_string()),
            status: DatasetVersionStatus::Active,
        };
        let json = serde_json::to_string(&dv).unwrap();
        let back: DatasetVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dataset_name, "ledger_entries");
        assert_eq!(back.version, 1);
        assert_eq!(back.status, DatasetVersionStatus::Active);
    }

    #[test]
    fn dataset_version_status_default_is_active() {
        assert_eq!(
            DatasetVersionStatus::default(),
            DatasetVersionStatus::Active
        );
    }

    #[test]
    fn dataset_version_status_serde_roundtrip() {
        for (variant, expected_str) in [
            (DatasetVersionStatus::Active, "\"active\""),
            (DatasetVersionStatus::Superseded, "\"superseded\""),
            (DatasetVersionStatus::Failed, "\"failed\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: DatasetVersionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn dataset_version_status_from_str() {
        assert_eq!(
            DatasetVersionStatus::from_str("active").unwrap(),
            DatasetVersionStatus::Active
        );
        assert_eq!(
            DatasetVersionStatus::from_str("superseded").unwrap(),
            DatasetVersionStatus::Superseded
        );
        assert_eq!(
            DatasetVersionStatus::from_str("failed").unwrap(),
            DatasetVersionStatus::Failed
        );
        assert!(DatasetVersionStatus::from_str("unknown").is_err());
    }

    #[test]
    fn dataset_version_status_display() {
        assert_eq!(DatasetVersionStatus::Active.to_string(), "active");
        assert_eq!(DatasetVersionStatus::Superseded.to_string(), "superseded");
        assert_eq!(DatasetVersionStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn dataset_version_backward_compat_missing_status() {
        // When status is absent from JSON, it should default to Active.
        let json = r#"{
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "dataset_name": "ledger_entries",
            "version": 1,
            "parser_hash": null,
            "created_at": "2026-01-01T00:00:00Z",
            "notes": null
        }"#;
        let dv: DatasetVersion = serde_json::from_str(json).unwrap();
        assert_eq!(dv.status, DatasetVersionStatus::Active);
    }

    // -- CompletenessStatus --

    #[test]
    fn completeness_status_default_is_partial() {
        assert_eq!(CompletenessStatus::default(), CompletenessStatus::Partial);
    }

    #[test]
    fn completeness_status_serde_roundtrip() {
        for (variant, expected_str) in [
            (CompletenessStatus::Partial, "\"partial\""),
            (CompletenessStatus::Complete, "\"complete\""),
            (CompletenessStatus::Backfilling, "\"backfilling\""),
            (CompletenessStatus::Gap, "\"gap\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: CompletenessStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn completeness_status_display() {
        assert_eq!(CompletenessStatus::Partial.to_string(), "partial");
        assert_eq!(CompletenessStatus::Complete.to_string(), "complete");
        assert_eq!(CompletenessStatus::Backfilling.to_string(), "backfilling");
        assert_eq!(CompletenessStatus::Gap.to_string(), "gap");
    }

    #[test]
    fn completeness_status_from_str() {
        assert_eq!(
            CompletenessStatus::from_str("partial").unwrap(),
            CompletenessStatus::Partial
        );
        assert_eq!(
            CompletenessStatus::from_str("complete").unwrap(),
            CompletenessStatus::Complete
        );
        assert_eq!(
            CompletenessStatus::from_str("backfilling").unwrap(),
            CompletenessStatus::Backfilling
        );
        assert_eq!(
            CompletenessStatus::from_str("gap").unwrap(),
            CompletenessStatus::Gap
        );
        assert!(CompletenessStatus::from_str("unknown").is_err());
    }

    // -- DatasetCompleteness --

    #[test]
    fn dataset_completeness_serde_roundtrip() {
        let now = Utc::now();
        let dc = DatasetCompleteness {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            dataset_name: "token_transfers".to_string(),
            dataset_version_id: Some(Uuid::new_v4()),
            network: "solana-mainnet".to_string(),
            status: CompletenessStatus::Partial,
            coverage_start: Some(1700000000),
            coverage_end: Some(1700100000),
            block_start: Some(200_000_000),
            block_end: Some(200_100_000),
            last_ingestion_run_id: Some(Uuid::new_v4()),
            records_count: 42,
            gap_ranges: Some(serde_json::json!([{"start": 1700050000, "end": 1700060000}])),
            notes: Some("initial partial ingest".to_string()),
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&dc).unwrap();
        let back: DatasetCompleteness = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dataset_name, "token_transfers");
        assert_eq!(back.network, "solana-mainnet");
        assert_eq!(back.status, CompletenessStatus::Partial);
        assert_eq!(back.records_count, 42);
        assert_eq!(back.coverage_start, Some(1700000000));
        assert_eq!(back.coverage_end, Some(1700100000));
        assert!(back.gap_ranges.is_some());
    }

    #[test]
    fn dataset_completeness_minimal_construction() {
        let now = Utc::now();
        let dc = DatasetCompleteness {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            dataset_name: "ledger_entries".to_string(),
            dataset_version_id: None,
            network: "ethereum-mainnet".to_string(),
            status: CompletenessStatus::default(),
            coverage_start: None,
            coverage_end: None,
            block_start: None,
            block_end: None,
            last_ingestion_run_id: None,
            records_count: 0,
            gap_ranges: None,
            notes: None,
            created_at: now,
            updated_at: now,
        };
        assert_eq!(dc.status, CompletenessStatus::Partial);
        assert_eq!(dc.records_count, 0);
        assert!(dc.coverage_start.is_none());
        assert!(dc.gap_ranges.is_none());
    }

    #[test]
    fn dataset_completeness_status_transitions() {
        let statuses = [
            CompletenessStatus::Partial,
            CompletenessStatus::Backfilling,
            CompletenessStatus::Complete,
        ];
        // Verify the natural progression serializes correctly
        for (i, status) in statuses.iter().enumerate() {
            let json = serde_json::to_string(status).unwrap();
            let back: CompletenessStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, status, "transition step {i}");
        }
    }

    #[test]
    fn dataset_completeness_no_wallet_specific_fields() {
        let now = Utc::now();
        let dc = DatasetCompleteness {
            id: Uuid::new_v4(),
            target_id: Uuid::new_v4(),
            dataset_name: "hl_fills".to_string(),
            dataset_version_id: None,
            network: "hypercore-mainnet".to_string(),
            status: CompletenessStatus::Complete,
            coverage_start: Some(1700000000),
            coverage_end: Some(1700100000),
            block_start: None,
            block_end: None,
            last_ingestion_run_id: None,
            records_count: 100,
            gap_ranges: None,
            notes: None,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&dc).unwrap();
        assert!(
            !json.contains("wallet_address"),
            "DatasetCompleteness must not have wallet_address"
        );
        assert!(
            !json.contains("user_id"),
            "DatasetCompleteness must not have user_id"
        );
    }

    #[test]
    fn ingestion_batch_serde_roundtrip() {
        let batch = IngestionBatch {
            records: vec![RawTransaction {
                id: Uuid::new_v4(),
                network: "base-mainnet".to_string(),
                tx_hash: "0x1234".to_string(),
                timestamp: 1700000000,
                block_number: Some(1_000_000),
                raw_metadata: serde_json::json!({}),
                source: "rpc".to_string(),
                ingestion_run_id: None,
                ingested_at: Utc::now(),
            }],
            checkpoint: None,
            run_metadata: None,
        };
        let json = serde_json::to_string(&batch).unwrap();
        let back: IngestionBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.records.len(), 1);
        assert_eq!(back.records[0].network, "base-mainnet");
    }

    // -- EvmTraceType --

    #[test]
    fn evm_trace_type_serde_roundtrip() {
        for (variant, expected_str) in [
            (EvmTraceType::CallTracer, "\"callTracer\""),
            (EvmTraceType::PrestateTracer, "\"prestateTracer\""),
            (EvmTraceType::FlatCallTracer, "\"flatCallTracer\""),
            (EvmTraceType::Other, "\"other\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: EvmTraceType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn evm_trace_type_display() {
        assert_eq!(EvmTraceType::CallTracer.to_string(), "callTracer");
        assert_eq!(EvmTraceType::PrestateTracer.to_string(), "prestateTracer");
        assert_eq!(EvmTraceType::FlatCallTracer.to_string(), "flatCallTracer");
        assert_eq!(EvmTraceType::Other.to_string(), "other");
    }

    #[test]
    fn evm_trace_type_from_str() {
        assert_eq!(
            EvmTraceType::from_str("callTracer").unwrap(),
            EvmTraceType::CallTracer
        );
        assert_eq!(
            EvmTraceType::from_str("prestateTracer").unwrap(),
            EvmTraceType::PrestateTracer
        );
        assert_eq!(
            EvmTraceType::from_str("flatCallTracer").unwrap(),
            EvmTraceType::FlatCallTracer
        );
        assert_eq!(
            EvmTraceType::from_str("other").unwrap(),
            EvmTraceType::Other
        );
        assert!(EvmTraceType::from_str("unknown").is_err());
    }

    // -- RawEvmTrace --

    #[test]
    fn raw_evm_trace_serde_roundtrip() {
        let trace = RawEvmTrace {
            id: Uuid::new_v4(),
            transaction_hash: "0xdeadbeef".to_string(),
            block_number: Some(18_000_000),
            network: "ethereum-mainnet".to_string(),
            trace_type: EvmTraceType::CallTracer,
            raw_trace: serde_json::json!({
                "type": "CALL",
                "from": "0x1111111111111111111111111111111111111111",
                "to": "0x2222222222222222222222222222222222222222",
                "value": "0xde0b6b3a7640000",
                "gas": "0x5208",
                "gasUsed": "0x5208",
                "input": "0x",
                "output": "0x",
            }),
            ingestion_run_id: None,
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&trace).unwrap();
        let back: RawEvmTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(back.transaction_hash, "0xdeadbeef");
        assert_eq!(back.network, "ethereum-mainnet");
        assert_eq!(back.trace_type, EvmTraceType::CallTracer);
        assert_eq!(back.block_number, Some(18_000_000));
        assert!(back.raw_trace.get("type").is_some());
    }

    #[test]
    fn raw_evm_trace_no_wallet_or_user_fields() {
        let trace = RawEvmTrace {
            id: Uuid::new_v4(),
            transaction_hash: "0xabc".to_string(),
            block_number: None,
            network: "base-mainnet".to_string(),
            trace_type: EvmTraceType::PrestateTracer,
            raw_trace: serde_json::json!({}),
            ingestion_run_id: Some(Uuid::new_v4()),
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&trace).unwrap();
        assert!(!json.contains("wallet_address"));
        assert!(!json.contains("user_id"));
    }

    #[test]
    fn raw_evm_trace_prestate_tracer_format() {
        let trace = RawEvmTrace {
            id: Uuid::new_v4(),
            transaction_hash: "0xabc123".to_string(),
            block_number: Some(19_000_000),
            network: "ethereum-mainnet".to_string(),
            trace_type: EvmTraceType::PrestateTracer,
            raw_trace: serde_json::json!({
                "pre": {
                    "0x1111111111111111111111111111111111111111": {
                        "balance": "0xde0b6b3a7640000",
                        "nonce": 42
                    }
                },
                "post": {
                    "0x1111111111111111111111111111111111111111": {
                        "balance": "0xc9f2c9cd04674edea40000000",
                        "nonce": 43
                    }
                }
            }),
            ingestion_run_id: None,
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&trace).unwrap();
        let back: RawEvmTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(back.trace_type, EvmTraceType::PrestateTracer);
        assert!(back.raw_trace.get("pre").is_some());
        assert!(back.raw_trace.get("post").is_some());
    }

    // -- Durable Control Plane types --

    #[test]
    fn ingestion_job_status_serde_roundtrip() {
        for (variant, expected_str) in [
            (IngestionJobStatus::Pending, "\"pending\""),
            (IngestionJobStatus::Claimed, "\"claimed\""),
            (IngestionJobStatus::Running, "\"running\""),
            (IngestionJobStatus::Completed, "\"completed\""),
            (IngestionJobStatus::Failed, "\"failed\""),
            (IngestionJobStatus::Cancelled, "\"cancelled\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: IngestionJobStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn ingestion_job_status_default_is_pending() {
        assert_eq!(IngestionJobStatus::default(), IngestionJobStatus::Pending);
    }

    #[test]
    fn ingestion_job_mode_serde_roundtrip() {
        for (variant, expected_str) in [
            (IngestionJobMode::Backfill, "\"backfill\""),
            (IngestionJobMode::Incremental, "\"incremental\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: IngestionJobMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn ingestion_job_serde_roundtrip() {
        let now = Utc::now();
        let job = IngestionJob {
            id: Uuid::new_v4(),
            target_id: Some(Uuid::new_v4()),
            network: "solana-mainnet".to_string(),
            mode: IngestionJobMode::Backfill,
            status: IngestionJobStatus::Pending,
            priority: 5,
            idempotency_key: Some("dedup-key-123".to_string()),
            requested_by: Some("api".to_string()),
            callback_url: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: IngestionJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.network, "solana-mainnet");
        assert_eq!(back.mode, IngestionJobMode::Backfill);
        assert_eq!(back.status, IngestionJobStatus::Pending);
        assert_eq!(back.priority, 5);
    }

    #[test]
    fn stream_desired_status_serde_roundtrip() {
        for (variant, expected_str) in [
            (StreamDesiredStatus::Active, "\"active\""),
            (StreamDesiredStatus::Paused, "\"paused\""),
            (StreamDesiredStatus::Stopped, "\"stopped\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: StreamDesiredStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn stream_actual_status_serde_roundtrip() {
        for (variant, expected_str) in [
            (StreamActualStatus::Pending, "\"pending\""),
            (StreamActualStatus::Running, "\"running\""),
            (StreamActualStatus::Paused, "\"paused\""),
            (StreamActualStatus::Stopped, "\"stopped\""),
            (StreamActualStatus::Error, "\"error\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: StreamActualStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn stream_source_serde_roundtrip() {
        for (variant, expected_str) in [
            (StreamSource::Grpc, "\"grpc\""),
            (StreamSource::Ws, "\"ws\""),
            (StreamSource::Rpc, "\"rpc\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: StreamSource = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn stream_subscription_serde_roundtrip() {
        let now = Utc::now();
        let sub = StreamSubscription {
            id: Uuid::new_v4(),
            target_id: Some(Uuid::new_v4()),
            network: "solana-mainnet".to_string(),
            source: StreamSource::Grpc,
            desired_status: StreamDesiredStatus::Active,
            actual_status: StreamActualStatus::Running,
            lease_owner: Some("worker-1".to_string()),
            heartbeat_at: Some(now),
            cursor_state: Some(serde_json::json!({"last_slot": 300})),
            config: None,
            error_message: None,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&sub).unwrap();
        let back: StreamSubscription = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, StreamSource::Grpc);
        assert_eq!(back.desired_status, StreamDesiredStatus::Active);
        assert_eq!(back.actual_status, StreamActualStatus::Running);
    }

    #[test]
    fn export_job_status_serde_roundtrip() {
        for (variant, expected_str) in [
            (ExportJobStatus::Pending, "\"pending\""),
            (ExportJobStatus::Running, "\"running\""),
            (ExportJobStatus::Delivering, "\"delivering\""),
            (ExportJobStatus::Completed, "\"completed\""),
            (ExportJobStatus::Failed, "\"failed\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: ExportJobStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn export_job_serde_roundtrip() {
        let now = Utc::now();
        let job = ExportJob {
            id: Uuid::new_v4(),
            dataset: "token_transfers".to_string(),
            format: "csv".to_string(),
            filters: Some(serde_json::json!({"network": "solana-mainnet"})),
            sink_config: None,
            status: ExportJobStatus::Pending,
            worker_id: None,
            record_count: None,
            result_location: None,
            error_message: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
            heartbeat_at: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let back: ExportJob = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dataset, "token_transfers");
        assert_eq!(back.format, "csv");
        assert_eq!(back.status, ExportJobStatus::Pending);
        assert!(back.heartbeat_at.is_none());
    }

    #[test]
    fn materialization_run_status_serde_roundtrip() {
        for (variant, expected_str) in [
            (MaterializationRunStatus::Pending, "\"pending\""),
            (MaterializationRunStatus::Running, "\"running\""),
            (MaterializationRunStatus::Completed, "\"completed\""),
            (MaterializationRunStatus::Failed, "\"failed\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_str, "serialize {variant:?}");
            let back: MaterializationRunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_str}");
        }
    }

    #[test]
    fn materialization_run_serde_roundtrip() {
        let now = Utc::now();
        let run = MaterializationRun {
            id: Uuid::new_v4(),
            dataset_name: "token_transfers".to_string(),
            scope: Some(serde_json::json!({"network": "solana-mainnet"})),
            input_watermark: Some(1700000000),
            output_record_count: 100,
            status: MaterializationRunStatus::Completed,
            dataset_version_id: Some(Uuid::new_v4()),
            worker_id: Some("worker-1".to_string()),
            started_at: Some(now),
            finished_at: Some(now),
            error_message: None,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&run).unwrap();
        let back: MaterializationRun = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dataset_name, "token_transfers");
        assert_eq!(back.output_record_count, 100);
        assert_eq!(back.status, MaterializationRunStatus::Completed);
    }
}
