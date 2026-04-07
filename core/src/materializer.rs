//! Dataset registry, Materializer trait, and regeneration types.
//!
//! This module defines canonical dataset identifiers, the `Materializer` trait
//! for parser/materializer version tracking, the types needed for
//! Bronze-to-Silver regeneration, and Silver dataset record structs.

use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString};
use uuid::Uuid;

use crate::v2::ChainFamily;

// ---------------------------------------------------------------------------
// Dataset Name
// ---------------------------------------------------------------------------

/// Canonical dataset identifiers.
///
/// Silver datasets correspond to the seven datasets defined in
/// V2_ARCHITECTURE_RFC Section 3.5. Gold datasets (`WalletLedger`,
/// `BalanceHistory`) are materialized from Silver data for downstream
/// wallet, tax, and forensics consumers.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString, EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DatasetName {
    /// Financial ledger for tax/portfolio (existing Silver dataset).
    LedgerEntries,
    /// Canonical token transfer records across chains.
    TokenTransfers,
    /// Native currency balance changes per account per transaction.
    NativeBalanceDeltas,
    /// ABI-decoded EVM events and Solana instruction logs.
    DecodedEvents,
    /// Hyperliquid fill records.
    HlFills,
    /// Hyperliquid funding payments.
    HlFunding,
    /// Position state changes derived from fills, funding, liquidations.
    Positions,
    /// Gold-tier wallet ledger with counterparty tracking (P5-W1).
    WalletLedger,
    /// Gold-tier per-asset balance history snapshots (P5-W1).
    BalanceHistory,
    /// Gold-tier Hyperliquid PnL summary per coin per period (P5-W2).
    HlPnlSummary,
    /// Gold-tier Hyperliquid trade history with entry/exit grouping (P5-W2).
    HlTradeHistory,
    /// Gold-tier protocol event records derived from decoded_events (P5-W3).
    ProtocolEvents,
    /// Gold-tier pool snapshot records derived from decoded_events + token_transfers (P5-W3).
    PoolSnapshots,
}

impl DatasetName {
    /// Returns the canonical public name as a SQL-compatible string.
    ///
    /// This is the name used in API routes, dataset versions, completeness
    /// tracking, and export metadata. It matches the `Display`/`FromStr`
    /// serialization of the enum.
    pub fn as_sql_str(&self) -> &'static str {
        match self {
            DatasetName::LedgerEntries => "ledger_entries",
            DatasetName::TokenTransfers => "token_transfers",
            DatasetName::NativeBalanceDeltas => "native_balance_deltas",
            DatasetName::DecodedEvents => "decoded_events",
            DatasetName::HlFills => "hl_fills",
            DatasetName::HlFunding => "hl_funding",
            DatasetName::Positions => "positions",
            DatasetName::WalletLedger => "wallet_ledger",
            DatasetName::BalanceHistory => "balance_history",
            DatasetName::HlPnlSummary => "hl_pnl_summary",
            DatasetName::HlTradeHistory => "hl_trade_history",
            DatasetName::ProtocolEvents => "protocol_events",
            DatasetName::PoolSnapshots => "pool_snapshots",
        }
    }

    /// Returns the physical PostgreSQL table name for this dataset.
    ///
    /// Most datasets share their canonical and physical names. The three
    /// Hyperliquid Silver datasets have different physical table names:
    ///
    /// | Canonical          | Physical               |
    /// |--------------------|------------------------|
    /// | `hl_fills`         | `hl_fill_records`      |
    /// | `hl_funding`       | `hl_funding_payments`  |
    /// | `positions`        | `hl_position_changes`  |
    pub fn physical_table(&self) -> &'static str {
        match self {
            DatasetName::LedgerEntries => "ledger_entries",
            DatasetName::TokenTransfers => "token_transfers",
            DatasetName::NativeBalanceDeltas => "native_balance_deltas",
            DatasetName::DecodedEvents => "decoded_events",
            DatasetName::HlFills => "hl_fill_records",
            DatasetName::HlFunding => "hl_funding_payments",
            DatasetName::Positions => "hl_position_changes",
            DatasetName::WalletLedger => "wallet_ledger",
            DatasetName::BalanceHistory => "balance_history",
            DatasetName::HlPnlSummary => "hl_pnl_summary",
            DatasetName::HlTradeHistory => "hl_trade_history",
            DatasetName::ProtocolEvents => "protocol_events",
            DatasetName::PoolSnapshots => "pool_snapshots",
        }
    }

    /// Resolve a `DatasetName` from a physical table name.
    ///
    /// Returns `None` if the table name does not correspond to any known
    /// dataset.  This is the reverse of [`physical_table`](Self::physical_table).
    pub fn from_physical_table(table: &str) -> Option<DatasetName> {
        // Check physical names that differ from canonical names first.
        match table {
            "hl_fill_records" => return Some(DatasetName::HlFills),
            "hl_funding_payments" => return Some(DatasetName::HlFunding),
            "hl_position_changes" => return Some(DatasetName::Positions),
            _ => {}
        }
        // For all others, canonical name == physical table name, so FromStr works.
        table.parse::<DatasetName>().ok()
    }

    /// Returns the dataset tier (Bronze, Silver, or Gold).
    pub fn tier(&self) -> DatasetTier {
        match self {
            DatasetName::LedgerEntries
            | DatasetName::TokenTransfers
            | DatasetName::NativeBalanceDeltas
            | DatasetName::DecodedEvents
            | DatasetName::HlFills
            | DatasetName::HlFunding
            | DatasetName::Positions => DatasetTier::Silver,

            DatasetName::WalletLedger
            | DatasetName::BalanceHistory
            | DatasetName::HlPnlSummary
            | DatasetName::HlTradeHistory
            | DatasetName::ProtocolEvents
            | DatasetName::PoolSnapshots => DatasetTier::Gold,
        }
    }

    /// Returns the chain families that can produce this dataset.
    pub fn chain_families(&self) -> &'static [ChainFamily] {
        match self {
            DatasetName::TokenTransfers => &[
                ChainFamily::Solana,
                ChainFamily::Evm,
                ChainFamily::Hyperliquid,
            ],
            // EVM native balance deltas require trace infrastructure that is
            // not yet wired into the materializer. Add ChainFamily::Evm here
            // once the trace-based materializer in evm_parser.rs is enabled.
            DatasetName::NativeBalanceDeltas => &[ChainFamily::Solana, ChainFamily::Hyperliquid],
            DatasetName::DecodedEvents => &[ChainFamily::Solana, ChainFamily::Evm],
            DatasetName::LedgerEntries => &[
                ChainFamily::Solana,
                ChainFamily::Evm,
                ChainFamily::Hyperliquid,
            ],
            DatasetName::HlFills | DatasetName::HlFunding | DatasetName::Positions => {
                &[ChainFamily::Hyperliquid]
            }
            DatasetName::WalletLedger | DatasetName::BalanceHistory => &[
                ChainFamily::Solana,
                ChainFamily::Evm,
                ChainFamily::Hyperliquid,
            ],
            DatasetName::HlPnlSummary | DatasetName::HlTradeHistory => &[ChainFamily::Hyperliquid],
            DatasetName::ProtocolEvents | DatasetName::PoolSnapshots => &[ChainFamily::Evm],
        }
    }

    /// Returns all dataset names as a slice.
    pub fn all() -> &'static [DatasetName] {
        &[
            DatasetName::LedgerEntries,
            DatasetName::TokenTransfers,
            DatasetName::NativeBalanceDeltas,
            DatasetName::DecodedEvents,
            DatasetName::HlFills,
            DatasetName::HlFunding,
            DatasetName::Positions,
            DatasetName::WalletLedger,
            DatasetName::BalanceHistory,
            DatasetName::HlPnlSummary,
            DatasetName::HlTradeHistory,
            DatasetName::ProtocolEvents,
            DatasetName::PoolSnapshots,
        ]
    }

    /// Returns all Silver-tier dataset names.
    pub fn silver() -> &'static [DatasetName] {
        &[
            DatasetName::LedgerEntries,
            DatasetName::TokenTransfers,
            DatasetName::NativeBalanceDeltas,
            DatasetName::DecodedEvents,
            DatasetName::HlFills,
            DatasetName::HlFunding,
            DatasetName::Positions,
        ]
    }

    /// Returns all Gold-tier dataset names.
    pub fn gold() -> &'static [DatasetName] {
        &[
            DatasetName::WalletLedger,
            DatasetName::BalanceHistory,
            DatasetName::HlPnlSummary,
            DatasetName::HlTradeHistory,
            DatasetName::ProtocolEvents,
            DatasetName::PoolSnapshots,
        ]
    }
}

// ---------------------------------------------------------------------------
// Dataset Tier
// ---------------------------------------------------------------------------

/// Classification tier for datasets in the Bronze-Silver-Gold model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum DatasetTier {
    /// Raw canonical chain data.
    Bronze,
    /// Normalized records derived from Bronze.
    Silver,
    /// Derived/aggregated datasets for specific use cases.
    Gold,
}

// ---------------------------------------------------------------------------
// Dataset Registry
// ---------------------------------------------------------------------------

/// Central dataset registry providing canonical name resolution and metadata.
///
/// The registry is the single source of truth for mapping between:
/// - canonical public dataset names (used in APIs, versions, completeness)
/// - physical PostgreSQL table names (used in SQL queries)
/// - dataset tier classification
/// - chain family support
/// - query/export capability
///
/// All dataset metadata lookups should go through this registry instead of
/// using hardcoded string literals.
pub struct DatasetRegistry;

impl DatasetRegistry {
    /// Resolve a canonical dataset name from a string that could be either
    /// a canonical name or a physical table name.
    ///
    /// Tries canonical name parsing first, then falls back to physical table
    /// name lookup. Returns `None` if neither matches.
    pub fn resolve(name: &str) -> Option<DatasetName> {
        name.parse::<DatasetName>()
            .ok()
            .or_else(|| DatasetName::from_physical_table(name))
    }

    /// Returns all datasets that are queryable via the dataset records endpoint.
    ///
    /// Excludes `LedgerEntries` which is served through the legacy
    /// `/v1/ledger/:wallet` endpoint.
    pub fn queryable() -> &'static [DatasetName] {
        &[
            DatasetName::TokenTransfers,
            DatasetName::NativeBalanceDeltas,
            DatasetName::DecodedEvents,
            DatasetName::HlFills,
            DatasetName::HlFunding,
            DatasetName::Positions,
            DatasetName::WalletLedger,
            DatasetName::BalanceHistory,
            DatasetName::HlPnlSummary,
            DatasetName::HlTradeHistory,
            DatasetName::ProtocolEvents,
            DatasetName::PoolSnapshots,
        ]
    }

    /// Returns all datasets that support export.
    ///
    /// Excludes `LedgerEntries` which is served through the legacy
    /// `/v1/export/:wallet` endpoint.
    pub fn exportable() -> &'static [DatasetName] {
        &[
            DatasetName::TokenTransfers,
            DatasetName::NativeBalanceDeltas,
            DatasetName::DecodedEvents,
            DatasetName::HlFills,
            DatasetName::HlFunding,
            DatasetName::Positions,
            DatasetName::WalletLedger,
            DatasetName::BalanceHistory,
            DatasetName::HlPnlSummary,
            DatasetName::HlTradeHistory,
            DatasetName::ProtocolEvents,
            DatasetName::PoolSnapshots,
        ]
    }

    /// Derive the [`ChainFamily`] for a network identifier.
    ///
    /// Uses the network ID prefix to determine the family. The mapping
    /// mirrors the `chain_family` column seeded in the `networks` table
    /// (see `20260308000001_add_networks.sql`).
    pub fn chain_family_for_network(network: &str) -> Option<ChainFamily> {
        if network.starts_with("solana-") {
            Some(ChainFamily::Solana)
        } else if network.starts_with("ethereum-")
            || network.starts_with("base-")
            || network.starts_with("arbitrum-")
            || network.starts_with("hyperevm-")
            || network.starts_with("optimism-")
            || network.starts_with("polygon-")
            || network.starts_with("scroll-")
            || network.starts_with("zksync-")
            || network.starts_with("linea-")
            || network.starts_with("mantle-")
            || network.starts_with("blast-")
        {
            Some(ChainFamily::Evm)
        } else if network.starts_with("hypercore-") {
            Some(ChainFamily::Hyperliquid)
        } else {
            #[cfg(debug_assertions)]
            eprintln!("[WARN] Unrecognized network prefix in chain_family_for_network: {network}");
            None
        }
    }

    /// Returns the Silver datasets materializable for a given chain family.
    ///
    /// Filters [`silver_materializable()`](Self::silver_materializable) to
    /// datasets whose [`chain_families()`](DatasetName::chain_families)
    /// includes `family`.
    pub fn silver_datasets_for_family(family: ChainFamily) -> Vec<DatasetName> {
        Self::silver_materializable()
            .iter()
            .filter(|ds| ds.chain_families().contains(&family))
            .copied()
            .collect()
    }

    /// Returns the Silver datasets that should be materialized for a given
    /// network identifier.
    ///
    /// Derives the chain family from the network ID prefix and delegates to
    /// [`silver_datasets_for_family()`](Self::silver_datasets_for_family).
    /// Returns an empty vec for unrecognised networks.
    pub fn silver_datasets_for_network(network: &str) -> Vec<DatasetName> {
        match Self::chain_family_for_network(network) {
            Some(family) => Self::silver_datasets_for_family(family),
            None => Vec::new(),
        }
    }

    /// Returns the Silver datasets that need version tracking during
    /// materialization.
    ///
    /// These are the datasets whose versions are resolved and stamped
    /// during the dual-write Silver materialization path.
    pub fn silver_materializable() -> &'static [DatasetName] {
        &[
            DatasetName::TokenTransfers,
            DatasetName::NativeBalanceDeltas,
            DatasetName::DecodedEvents,
            DatasetName::HlFills,
            DatasetName::HlFunding,
            DatasetName::Positions,
        ]
    }

    /// Check whether a canonical name string is a known queryable dataset.
    pub fn is_queryable(name: &str) -> bool {
        Self::queryable().iter().any(|ds| ds.as_sql_str() == name)
    }

    /// Check whether a canonical name string is a known exportable dataset.
    pub fn is_exportable(name: &str) -> bool {
        Self::exportable().iter().any(|ds| ds.as_sql_str() == name)
    }
}

// ---------------------------------------------------------------------------
// Dataset Descriptor
// ---------------------------------------------------------------------------

/// Metadata describing a Silver dataset's purpose and lineage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetDescriptor {
    /// Canonical dataset identifier.
    pub name: DatasetName,
    /// Human-readable description.
    pub description: String,
    /// Bronze tables this dataset derives from.
    pub source_bronze_tables: Vec<String>,
    /// Chain families that can produce this dataset.
    pub chain_families: Vec<ChainFamily>,
}

impl DatasetDescriptor {
    /// Validate that the descriptor has a non-empty description and at least
    /// one source bronze table.
    pub fn validate(&self) -> Result<(), String> {
        if self.description.is_empty() {
            return Err("DatasetDescriptor description must not be empty".to_string());
        }
        if self.source_bronze_tables.is_empty() {
            return Err("DatasetDescriptor must have at least one source_bronze_table".to_string());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Materializer Trait
// ---------------------------------------------------------------------------

/// Trait for parser/materializer version tracking.
///
/// Each chain-specific parser that produces Silver data implements this trait
/// to declare its identity and version. This enables:
/// - safe reprocessing when parser logic changes
/// - dataset version linkage for provenance tracking
/// - regeneration decisions based on version comparison
pub trait Materializer: Send + Sync {
    /// The canonical dataset this materializer produces.
    fn dataset_name(&self) -> DatasetName;

    /// Monotonically increasing parser version number.
    /// Increment this when the parser logic changes.
    fn parser_version(&self) -> i32;

    /// Stable content hash of the parser logic.
    /// This should change whenever the parser behavior changes,
    /// enabling detection of unreleased changes during development.
    fn parser_hash(&self) -> &str;

    /// The chain family this materializer serves.
    fn chain_family(&self) -> ChainFamily;

    /// Build the dataset descriptor for this materializer's output.
    fn descriptor(&self) -> DatasetDescriptor;
}

// ---------------------------------------------------------------------------
// Regeneration Types
// ---------------------------------------------------------------------------

/// Scope of a regeneration request — which data to reprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegenerationScope {
    /// Dataset to regenerate.
    pub dataset: DatasetName,
    /// Optional target ID to scope regeneration.
    /// When `None`, regenerates across all targets.
    pub target_id: Option<uuid::Uuid>,
    /// Optional time range to scope regeneration.
    /// Both bounds are inclusive Unix timestamps.
    pub time_range: Option<(i64, i64)>,
}

impl RegenerationScope {
    /// Validate the scope. If a time range is provided, start must be <= end.
    pub fn validate(&self) -> Result<(), String> {
        if let Some((start, end)) = self.time_range {
            if start > end {
                return Err(format!(
                    "RegenerationScope time_range start ({start}) must be <= end ({end})"
                ));
            }
        }
        Ok(())
    }
}

/// A request to regenerate Silver data from Bronze.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegenerationRequest {
    /// What to regenerate.
    pub scope: RegenerationScope,
    /// Reason for regeneration (e.g. "parser upgrade v1→v2").
    pub reason: String,
    /// Whether to supersede the current version.
    pub supersede_current: bool,
}

// ---------------------------------------------------------------------------
// Sink Types
// ---------------------------------------------------------------------------

/// Supported export sink types.
///
/// Determines where completed export data is delivered. The default behavior
/// (no sink) stores data in-memory for download via the existing endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SinkType {
    /// Write export data to a local file path.
    LocalFile,
    /// POST export data to an HTTP(S) webhook URL.
    Webhook,
    /// Deliver export data to an external database (stub — not yet implemented at runtime).
    Database,
    // ObjectStorage is the planned next extension point.
    // It is intentionally not included as a variant yet to avoid dead code,
    // but the enum is designed to accommodate it without breaking changes.
}

/// Configuration for an export sink.
///
/// Each sink type has its own required fields. The API validates the config
/// at job creation time based on the `sink_type`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SinkConfig {
    /// Which sink type to deliver to.
    pub sink_type: SinkType,

    // -- LocalFile fields --
    /// Absolute file path for LocalFile sink output.
    pub file_path: Option<String>,

    // -- Webhook fields --
    /// HTTP(S) URL for Webhook sink delivery.
    pub url: Option<String>,
    /// Optional HTTP headers to include with the webhook POST.
    pub headers: Option<std::collections::HashMap<String, String>>,

    // -- Database fields --
    /// Connection string for Database sink (e.g. `postgresql://host/db`).
    pub connection_string: Option<String>,
    /// Target table name for Database sink.
    pub table: Option<String>,
}

impl SinkConfig {
    /// Validate that the config has the required fields for its sink type.
    pub fn validate(&self) -> Result<(), String> {
        match self.sink_type {
            SinkType::LocalFile => {
                let path = self
                    .file_path
                    .as_deref()
                    .ok_or("LocalFile sink requires 'file_path'")?;
                if path.is_empty() {
                    return Err("file_path must not be empty".to_string());
                }
                Ok(())
            }
            SinkType::Webhook => {
                let url = self.url.as_deref().ok_or("Webhook sink requires 'url'")?;
                if url.is_empty() {
                    return Err("url must not be empty".to_string());
                }
                Ok(())
            }
            SinkType::Database => {
                let cs = self
                    .connection_string
                    .as_deref()
                    .ok_or("Database sink requires 'connection_string'")?;
                if cs.is_empty() {
                    return Err("connection_string must not be empty".to_string());
                }
                let tbl = self
                    .table
                    .as_deref()
                    .ok_or("Database sink requires 'table'")?;
                if tbl.is_empty() {
                    return Err("table must not be empty".to_string());
                }
                Ok(())
            }
        }
    }
}

/// Metadata about an export delivery for receipt tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryMetadata {
    /// Job ID that produced the export data.
    pub job_id: Uuid,
    /// Dataset that was exported.
    pub dataset: String,
    /// Export format (e.g. "jsonl", "csv").
    pub format: String,
    /// Number of records in the export.
    pub record_count: usize,
    /// ID of the dataset version used to produce the export (provenance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset_version_id: Option<Uuid>,
    /// Completeness status of the dataset at export time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completeness_status: Option<String>,
}

/// Receipt returned after successful sink delivery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    /// Sink type that delivered the data.
    pub sink_type: SinkType,
    /// Human-readable description of where data was delivered.
    pub destination: String,
    /// Number of bytes delivered.
    pub bytes_written: usize,
    /// When delivery completed.
    pub delivered_at: DateTime<Utc>,
}

/// Async trait for export sink implementations.
///
/// Each sink type implements this trait to deliver serialized export data
/// to its destination. Sinks receive the raw bytes (already serialized as
/// JSONL or CSV) plus metadata about the export job.
#[async_trait::async_trait]
pub trait ExportSink: Send + Sync {
    /// Deliver export data to the sink destination.
    async fn deliver(
        &self,
        data: &[u8],
        metadata: &DeliveryMetadata,
    ) -> Result<DeliveryReceipt, String>;
}

// ---------------------------------------------------------------------------
// Export Format
// ---------------------------------------------------------------------------

/// Supported export output formats.
///
/// Used by P4-W2 export jobs to specify how dataset records should be
/// serialized. JSONL and CSV are supported now; Parquet can be added later
/// without changing the request model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ExportFormat {
    /// Newline-delimited JSON (one JSON object per line).
    Jsonl,
    /// Comma-separated values with a header row.
    Csv,
}

// ---------------------------------------------------------------------------
// Silver Dataset Records
// ---------------------------------------------------------------------------

/// Normalized token transfer record across all chains.
///
/// Each record represents a single token movement extracted from Bronze data.
/// Addresses are generic (not wallet-specific) to support contract, program,
/// and protocol target types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenTransfer {
    pub id: Uuid,
    /// FK to raw_transactions; nullable during transition.
    pub raw_transaction_id: Option<Uuid>,
    pub network: String,
    /// Token contract/mint address (e.g. SPL mint, ERC-20 contract).
    pub token_address: String,
    /// Human-readable symbol if resolvable; otherwise the token address.
    pub token_symbol: Option<String>,
    /// Sender address.
    pub from_address: String,
    /// Receiver address.
    pub to_address: String,
    /// Transfer amount (positive = movement from sender to receiver).
    pub amount: BigDecimal,
    /// Token decimals used for normalization.
    pub decimals: i32,
    /// Ordinal position within the same (raw_transaction_id, from, to, token) group.
    pub transfer_index: i32,
    /// FK to dataset_versions; nullable during transition.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Decoded event or instruction record across all chains.
///
/// Each record represents a single decoded log event (EVM) or program
/// instruction (Solana) extracted from Bronze data. Not wallet-specific —
/// any contract interaction or program invocation can produce decoded events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodedEvent {
    pub id: Uuid,
    /// FK to raw_transactions; nullable during transition.
    pub raw_transaction_id: Option<Uuid>,
    pub network: String,
    /// Contract address (EVM) or program ID (Solana).
    pub program_or_contract: String,
    /// Event signature hash (EVM topic0) or instruction discriminator.
    pub event_signature: Option<String>,
    /// Human-readable event name if decodable (e.g. "Transfer", "Approval").
    pub event_name: Option<String>,
    /// Position within the transaction (log index for EVM, instruction index for Solana).
    pub log_index: i32,
    /// Structured decoded fields (named parameters when ABI is known).
    pub decoded_fields: serde_json::Value,
    /// Raw field data (topics + data for EVM, instruction bytes for Solana).
    pub raw_fields: serde_json::Value,
    /// FK to dataset_versions; nullable during transition.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Native currency balance delta per account per transaction.
///
/// Each record represents the change in native token balance for a single
/// account in a single transaction. Not wallet-specific — any account
/// involved in a transaction can have a delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeBalanceDelta {
    pub id: Uuid,
    /// FK to raw_transactions; nullable during transition.
    pub raw_transaction_id: Option<Uuid>,
    pub network: String,
    /// The account whose balance changed.
    pub account_address: String,
    /// Native token symbol (e.g. "SOL", "ETH", "USDC" for Hyperliquid).
    pub native_token: String,
    /// Balance before the transaction.
    pub pre_balance: BigDecimal,
    /// Balance after the transaction.
    pub post_balance: BigDecimal,
    /// Computed delta (post_balance - pre_balance).
    pub delta: BigDecimal,
    /// Whether this account was the fee payer for the transaction.
    pub is_fee_payer: bool,
    /// FK to dataset_versions; nullable during transition.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Normalized Hyperliquid fill record.
///
/// Each record represents a single fill (trade execution) on Hyperliquid,
/// extracted from Bronze raw_transactions metadata. Not wallet-specific —
/// any account's fills can be represented.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlFillRecord {
    pub id: Uuid,
    /// FK to raw_transactions; nullable during transition.
    pub raw_transaction_id: Option<Uuid>,
    pub network: String,
    /// The traded asset (e.g. "ETH", "BTC").
    pub coin: String,
    /// Trade side: "B" (buy) or "A"/"S" (sell).
    pub side: String,
    /// Execution price as a decimal string.
    pub price: BigDecimal,
    /// Fill size (quantity).
    pub size: BigDecimal,
    /// Trade direction (e.g. "Open Long", "Close Short").
    pub direction: Option<String>,
    /// Realized PnL from closing a position.
    pub closed_pnl: Option<BigDecimal>,
    /// Fee charged for this fill.
    pub fee: Option<BigDecimal>,
    /// Token used to pay the fee (typically "USDC").
    pub fee_token: Option<String>,
    /// Timestamp of the fill (milliseconds since epoch).
    pub fill_time: i64,
    /// Hyperliquid order ID.
    pub order_id: Option<i64>,
    /// Hyperliquid trade ID.
    pub trade_id: Option<i64>,
    /// FK to dataset_versions; nullable during transition.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Normalized Hyperliquid funding payment record.
///
/// Each record represents a single funding payment received or paid on
/// Hyperliquid. Not wallet-specific — any account's funding payments
/// can be represented.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlFundingPayment {
    pub id: Uuid,
    /// FK to raw_transactions; nullable during transition.
    pub raw_transaction_id: Option<Uuid>,
    pub network: String,
    /// The asset the funding rate applies to (e.g. "ETH").
    pub coin: String,
    /// USDC amount: positive = received, negative = paid.
    pub amount: BigDecimal,
    /// The funding rate applied.
    pub funding_rate: Option<BigDecimal>,
    /// Timestamp of the funding payment (milliseconds since epoch).
    pub payment_time: i64,
    /// FK to dataset_versions; nullable during transition.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Normalized Hyperliquid position change record.
///
/// Each record represents a position state change derived from fills,
/// liquidations, or other events. Not wallet-specific — any account's
/// position changes can be represented.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlPositionChange {
    pub id: Uuid,
    /// FK to raw_transactions; nullable during transition.
    pub raw_transaction_id: Option<Uuid>,
    pub network: String,
    /// The asset whose position changed (e.g. "ETH").
    pub coin: String,
    /// Trade side that caused the change: "B" (buy) or "A"/"S" (sell).
    pub side: String,
    /// Size delta from this event (signed: positive = increase, negative = decrease).
    pub size_delta: BigDecimal,
    /// Execution price of the triggering event.
    pub price: BigDecimal,
    /// Direction of the position change (e.g. "Open Long", "Close Short").
    pub direction: Option<String>,
    /// Source event type: "fill", "liquidation".
    pub source_event: String,
    /// FK to dataset_versions; nullable during transition.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Gold Dataset Records (P5-W1)
// ---------------------------------------------------------------------------

/// Gold-tier wallet ledger record with counterparty tracking.
///
/// Materialized from Silver datasets (token_transfers, native_balance_deltas,
/// hl_fills, hl_funding). Each record represents a single financial event
/// for a specific wallet, enriched with counterparty address, fee breakdown,
/// and nullable cost basis / proceeds fields for downstream tax consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletLedgerRecord {
    pub id: Uuid,
    /// FK to raw_transactions; nullable during transition.
    pub raw_transaction_id: Option<Uuid>,
    /// The wallet this record belongs to.
    pub wallet_address: String,
    /// Network where the transaction occurred (e.g. "solana-mainnet").
    pub network: String,
    /// Transaction hash for on-chain reference.
    pub tx_hash: String,
    /// Unix timestamp (seconds or milliseconds depending on chain).
    pub timestamp: i64,
    /// Entry type: "trade", "fee", "transfer", "income", "funding".
    pub entry_type: String,
    /// Asset symbol (e.g. "SOL", "USDC", "ETH").
    pub asset_symbol: String,
    /// Signed amount (positive = inflow, negative = outflow).
    pub amount: BigDecimal,
    /// Address of the counterparty (sender for inflows, receiver for outflows).
    pub counterparty_address: Option<String>,
    /// Fee amount associated with this entry (if applicable).
    pub fee_amount: Option<BigDecimal>,
    /// Asset used to pay the fee.
    pub fee_asset: Option<String>,
    /// Cost basis in fiat (nullable — for future tax lot matching).
    pub cost_basis: Option<BigDecimal>,
    /// Proceeds in fiat (nullable — for future tax lot matching).
    pub proceeds: Option<BigDecimal>,
    /// FK to dataset_versions; nullable during transition.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Gold-tier balance history snapshot.
///
/// Derived from wallet_ledger entries or directly from Silver data.
/// Each record represents the running balance for a specific asset
/// in a specific wallet at a specific point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    pub id: Uuid,
    /// The wallet this snapshot belongs to.
    pub wallet_address: String,
    /// Asset symbol.
    pub asset_symbol: String,
    /// Network.
    pub network: String,
    /// Unix timestamp of the snapshot.
    pub timestamp: i64,
    /// Running balance after the referenced transaction.
    pub balance: BigDecimal,
    /// Transaction hash that caused this balance change.
    pub tx_hash: String,
    /// FK to dataset_versions; nullable during transition.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Forensics activity summary for a wallet.
///
/// Aggregated view of wallet interactions — top counterparties,
/// cross-chain summary, and transaction type breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForensicsActivity {
    /// Wallet being analyzed.
    pub wallet_address: String,
    /// Top counterparties by interaction count.
    pub top_counterparties: Vec<CounterpartySummary>,
    /// Activity breakdown by network.
    pub network_activity: Vec<NetworkActivity>,
    /// Transaction type breakdown.
    pub type_breakdown: Vec<TypeBreakdown>,
    /// Total number of wallet_ledger records.
    pub total_entries: usize,
}

/// Summary of interactions with a single counterparty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterpartySummary {
    pub address: String,
    pub interaction_count: usize,
    pub total_inflow: BigDecimal,
    pub total_outflow: BigDecimal,
    pub networks: Vec<String>,
}

/// Activity summary for a single network.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkActivity {
    pub network: String,
    pub entry_count: usize,
    pub unique_assets: usize,
    pub unique_counterparties: usize,
}

/// Breakdown of entries by type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeBreakdown {
    pub entry_type: String,
    pub count: usize,
    pub total_amount: BigDecimal,
}

// ---------------------------------------------------------------------------
// Gold Dataset Records (P5-W2): Hyperliquid Analytics
// ---------------------------------------------------------------------------

/// Gold-tier Hyperliquid PnL summary per wallet per coin per period.
///
/// Aggregated from Silver hl_fills (closed_pnl, fees) and hl_funding
/// (funding amounts). Each record summarizes performance metrics for a
/// single coin within a time period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlPnlSummary {
    pub id: Uuid,
    pub wallet_address: String,
    pub coin: String,
    pub network: String,
    pub period_start: i64,
    pub period_end: i64,
    pub total_closed_pnl: BigDecimal,
    pub total_funding: BigDecimal,
    pub total_fees: BigDecimal,
    pub net_pnl: BigDecimal,
    pub trade_count: i64,
    pub fill_count: i64,
    pub avg_trade_size: BigDecimal,
    pub win_count: i64,
    pub loss_count: i64,
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Gold-tier Hyperliquid trade history record.
///
/// Groups individual fills into logical trades with entry/exit prices
/// and realized PnL. Each record represents a single open→close trade
/// sequence for a coin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlTradeHistory {
    pub id: Uuid,
    pub wallet_address: String,
    pub coin: String,
    pub network: String,
    pub side: String,
    pub entry_price: BigDecimal,
    pub exit_price: BigDecimal,
    pub size: BigDecimal,
    pub opened_at: i64,
    pub closed_at: i64,
    pub realized_pnl: BigDecimal,
    pub fees: BigDecimal,
    pub num_fills: i64,
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Per-trader analytics response for the Hyperliquid analytics endpoint.
///
/// Dashboard-ready summary of a trader's performance across coins,
/// computed from Gold hl_pnl_summary and hl_trade_history records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraderAnalytics {
    pub wallet_address: String,
    pub total_net_pnl: BigDecimal,
    pub total_volume: BigDecimal,
    pub total_trades: i64,
    pub win_rate: f64,
    pub coin_breakdown: Vec<CoinPnlSummary>,
}

/// Per-coin PnL breakdown within a trader analytics response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoinPnlSummary {
    pub coin: String,
    pub net_pnl: BigDecimal,
    pub volume: BigDecimal,
    pub trade_count: i64,
    pub win_count: i64,
    pub loss_count: i64,
}

/// Per-coin market analytics response for the Hyperliquid analytics endpoint.
///
/// Aggregated view of trading activity for a single coin across all traders,
/// computed from Gold hl_pnl_summary and hl_trade_history records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketAnalytics {
    pub coins: Vec<CoinMarketSummary>,
    pub total_volume: BigDecimal,
    pub total_unique_traders: usize,
}

/// Per-coin aggregate market summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoinMarketSummary {
    pub coin: String,
    pub total_volume: BigDecimal,
    pub unique_traders: usize,
    pub total_trades: i64,
    pub total_pnl: BigDecimal,
    pub avg_trade_size: BigDecimal,
}

// ---------------------------------------------------------------------------
// Gold Dataset Records (P5-W3): Protocol / TVL
// ---------------------------------------------------------------------------

/// Gold-tier protocol event record.
///
/// Derived from Silver `decoded_events` by grouping events by their
/// `program_or_contract` as the protocol_address. Each record represents
/// a significant protocol-level event (swap, mint, burn, liquidity change).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolEvent {
    pub id: Uuid,
    pub network: String,
    /// Contract or program address acting as the protocol identifier.
    pub protocol_address: String,
    /// Human-readable protocol name (if resolvable).
    pub protocol_name: Option<String>,
    /// Event classification: "swap", "mint", "burn", "liquidity_added",
    /// "liquidity_removed", "transfer", "other".
    pub event_type: String,
    /// Structured event details (decoded fields snapshot).
    pub event_details: serde_json::Value,
    /// Pool or pair address involved in the event (if applicable).
    pub pool_address: Option<String>,
    /// FK to the source decoded_events record.
    pub raw_event_id: Option<Uuid>,
    /// Unix timestamp of the event.
    pub timestamp: i64,
    /// FK to dataset_versions.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Gold-tier pool snapshot record.
///
/// Derived from Silver `decoded_events` (swap / liquidity events) and
/// `token_transfers` to capture per-pool reserve state at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolSnapshot {
    pub id: Uuid,
    pub network: String,
    /// Pool or pair contract address.
    pub pool_address: String,
    /// Protocol address the pool belongs to.
    pub protocol_address: String,
    /// Human-readable protocol name (if resolvable).
    pub protocol_name: Option<String>,
    /// Address of the first token in the pair.
    pub token0_address: String,
    /// Symbol of the first token.
    pub token0_symbol: Option<String>,
    /// Address of the second token in the pair.
    pub token1_address: String,
    /// Symbol of the second token.
    pub token1_symbol: Option<String>,
    /// Reserve amount for token0.
    pub reserve0: BigDecimal,
    /// Reserve amount for token1.
    pub reserve1: BigDecimal,
    /// USD-denominated total value locked (nullable — requires price feed).
    pub tvl_usd: Option<BigDecimal>,
    /// Unix timestamp of the snapshot.
    pub snapshot_timestamp: i64,
    /// Block number at snapshot time.
    pub block_number: Option<i64>,
    /// FK to dataset_versions.
    pub dataset_version_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// Per-protocol activity analytics response.
///
/// Aggregated view of protocol interactions — event counts by type,
/// unique participant count, and time range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolActivity {
    /// Protocol contract or program address.
    pub protocol_address: String,
    /// Event counts grouped by event_type.
    pub event_counts_by_type: Vec<EventTypeCount>,
    /// Number of unique participant addresses.
    pub unique_participants: usize,
    /// Total number of protocol events.
    pub total_events: usize,
    /// Earliest event timestamp.
    pub time_start: Option<i64>,
    /// Latest event timestamp.
    pub time_end: Option<i64>,
}

/// Event count for a single event type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTypeCount {
    pub event_type: String,
    pub count: usize,
}

/// TVL analytics response.
///
/// Per-pool and aggregate TVL computed from pool_snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TvlAnalytics {
    /// Per-pool TVL snapshots.
    pub pools: Vec<PoolTvlSummary>,
    /// Aggregate TVL across all pools (sum of tvl_usd where available).
    pub total_tvl: Option<BigDecimal>,
    /// Protocol-level aggregation (grouped by protocol_address).
    pub protocols: Vec<ProtocolTvlSummary>,
}

/// TVL summary for a single pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolTvlSummary {
    pub pool_address: String,
    pub protocol_address: String,
    pub token0_symbol: Option<String>,
    pub token1_symbol: Option<String>,
    pub reserve0: BigDecimal,
    pub reserve1: BigDecimal,
    pub tvl_usd: Option<BigDecimal>,
    pub snapshot_timestamp: i64,
}

/// TVL summary for a protocol (sum across its pools).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolTvlSummary {
    pub protocol_address: String,
    pub protocol_name: Option<String>,
    pub pool_count: usize,
    pub total_tvl: Option<BigDecimal>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use strum::IntoEnumIterator;

    #[test]
    fn dataset_name_count_is_thirteen() {
        assert_eq!(DatasetName::all().len(), 13);
        assert_eq!(DatasetName::iter().count(), 13);
    }

    #[test]
    fn dataset_name_serde_roundtrip() {
        let cases = [
            (DatasetName::LedgerEntries, "\"ledger_entries\""),
            (DatasetName::TokenTransfers, "\"token_transfers\""),
            (
                DatasetName::NativeBalanceDeltas,
                "\"native_balance_deltas\"",
            ),
            (DatasetName::DecodedEvents, "\"decoded_events\""),
            (DatasetName::HlFills, "\"hl_fills\""),
            (DatasetName::HlFunding, "\"hl_funding\""),
            (DatasetName::Positions, "\"positions\""),
            (DatasetName::WalletLedger, "\"wallet_ledger\""),
            (DatasetName::BalanceHistory, "\"balance_history\""),
            (DatasetName::HlPnlSummary, "\"hl_pnl_summary\""),
            (DatasetName::HlTradeHistory, "\"hl_trade_history\""),
            (DatasetName::ProtocolEvents, "\"protocol_events\""),
            (DatasetName::PoolSnapshots, "\"pool_snapshots\""),
        ];
        assert_eq!(cases.len(), 13, "must cover all 13 datasets");
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected_json, "serialize {variant:?}");
            let back: DatasetName = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected_json}");
        }
    }

    #[test]
    fn dataset_name_display() {
        assert_eq!(DatasetName::LedgerEntries.to_string(), "ledger_entries");
        assert_eq!(DatasetName::TokenTransfers.to_string(), "token_transfers");
        assert_eq!(
            DatasetName::NativeBalanceDeltas.to_string(),
            "native_balance_deltas"
        );
        assert_eq!(DatasetName::DecodedEvents.to_string(), "decoded_events");
        assert_eq!(DatasetName::HlFills.to_string(), "hl_fills");
        assert_eq!(DatasetName::HlFunding.to_string(), "hl_funding");
        assert_eq!(DatasetName::Positions.to_string(), "positions");
        assert_eq!(DatasetName::WalletLedger.to_string(), "wallet_ledger");
        assert_eq!(DatasetName::BalanceHistory.to_string(), "balance_history");
        assert_eq!(DatasetName::HlPnlSummary.to_string(), "hl_pnl_summary");
        assert_eq!(DatasetName::HlTradeHistory.to_string(), "hl_trade_history");
        assert_eq!(DatasetName::ProtocolEvents.to_string(), "protocol_events");
        assert_eq!(DatasetName::PoolSnapshots.to_string(), "pool_snapshots");
    }

    #[test]
    fn dataset_name_from_str() {
        assert_eq!(
            DatasetName::from_str("ledger_entries").unwrap(),
            DatasetName::LedgerEntries
        );
        assert_eq!(
            DatasetName::from_str("token_transfers").unwrap(),
            DatasetName::TokenTransfers
        );
        assert_eq!(
            DatasetName::from_str("hl_fills").unwrap(),
            DatasetName::HlFills
        );
        assert_eq!(
            DatasetName::from_str("wallet_ledger").unwrap(),
            DatasetName::WalletLedger
        );
        assert_eq!(
            DatasetName::from_str("balance_history").unwrap(),
            DatasetName::BalanceHistory
        );
        assert_eq!(
            DatasetName::from_str("hl_pnl_summary").unwrap(),
            DatasetName::HlPnlSummary
        );
        assert_eq!(
            DatasetName::from_str("hl_trade_history").unwrap(),
            DatasetName::HlTradeHistory
        );
        assert_eq!(
            DatasetName::from_str("protocol_events").unwrap(),
            DatasetName::ProtocolEvents
        );
        assert_eq!(
            DatasetName::from_str("pool_snapshots").unwrap(),
            DatasetName::PoolSnapshots
        );
        assert!(DatasetName::from_str("unknown_dataset").is_err());
    }

    #[test]
    fn dataset_name_as_sql_str() {
        for name in DatasetName::all() {
            assert_eq!(
                name.as_sql_str(),
                name.to_string(),
                "as_sql_str should match Display for {name:?}"
            );
        }
    }

    #[test]
    fn dataset_descriptor_construction() {
        let desc = DatasetDescriptor {
            name: DatasetName::LedgerEntries,
            description: "Financial ledger".to_string(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![
                ChainFamily::Solana,
                ChainFamily::Evm,
                ChainFamily::Hyperliquid,
            ],
        };
        assert!(desc.validate().is_ok());
        let json = serde_json::to_string(&desc).unwrap();
        let back: DatasetDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, DatasetName::LedgerEntries);
        assert_eq!(back.source_bronze_tables.len(), 1);
        assert_eq!(back.chain_families.len(), 3);
    }

    #[test]
    fn dataset_descriptor_validation_empty_description() {
        let desc = DatasetDescriptor {
            name: DatasetName::TokenTransfers,
            description: String::new(),
            source_bronze_tables: vec!["raw_transactions".to_string()],
            chain_families: vec![ChainFamily::Evm],
        };
        assert!(desc.validate().is_err());
    }

    #[test]
    fn dataset_descriptor_validation_no_source_tables() {
        let desc = DatasetDescriptor {
            name: DatasetName::TokenTransfers,
            description: "Token transfers".to_string(),
            source_bronze_tables: vec![],
            chain_families: vec![ChainFamily::Evm],
        };
        assert!(desc.validate().is_err());
    }

    #[test]
    fn regeneration_scope_validation_valid() {
        let scope = RegenerationScope {
            dataset: DatasetName::LedgerEntries,
            target_id: None,
            time_range: Some((1000, 2000)),
        };
        assert!(scope.validate().is_ok());
    }

    #[test]
    fn regeneration_scope_validation_equal_bounds() {
        let scope = RegenerationScope {
            dataset: DatasetName::LedgerEntries,
            target_id: None,
            time_range: Some((1000, 1000)),
        };
        assert!(scope.validate().is_ok());
    }

    #[test]
    fn regeneration_scope_validation_no_time_range() {
        let scope = RegenerationScope {
            dataset: DatasetName::HlFills,
            target_id: Some(uuid::Uuid::new_v4()),
            time_range: None,
        };
        assert!(scope.validate().is_ok());
    }

    #[test]
    fn regeneration_scope_validation_invalid_time_range() {
        let scope = RegenerationScope {
            dataset: DatasetName::LedgerEntries,
            target_id: None,
            time_range: Some((2000, 1000)),
        };
        let err = scope.validate().unwrap_err();
        assert!(err.contains("start"));
        assert!(err.contains("end"));
    }

    #[test]
    fn regeneration_request_serde_roundtrip() {
        let req = RegenerationRequest {
            scope: RegenerationScope {
                dataset: DatasetName::TokenTransfers,
                target_id: Some(uuid::Uuid::new_v4()),
                time_range: Some((1000, 2000)),
            },
            reason: "parser upgrade v1→v2".to_string(),
            supersede_current: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: RegenerationRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.scope.dataset, DatasetName::TokenTransfers);
        assert!(back.supersede_current);
        assert_eq!(back.reason, "parser upgrade v1→v2");
    }

    // -- ExportFormat tests --

    #[test]
    fn export_format_serde_roundtrip() {
        for (variant, expected) in [
            (ExportFormat::Jsonl, "\"jsonl\""),
            (ExportFormat::Csv, "\"csv\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected, "serialize {variant:?}");
            let back: ExportFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected}");
        }
    }

    #[test]
    fn export_format_from_str() {
        assert_eq!(
            ExportFormat::from_str("jsonl").unwrap(),
            ExportFormat::Jsonl
        );
        assert_eq!(ExportFormat::from_str("csv").unwrap(), ExportFormat::Csv);
        assert!(ExportFormat::from_str("parquet").is_err());
        assert!(ExportFormat::from_str("xml").is_err());
    }

    #[test]
    fn export_format_display() {
        assert_eq!(ExportFormat::Jsonl.to_string(), "jsonl");
        assert_eq!(ExportFormat::Csv.to_string(), "csv");
    }

    // -- Materializer trait contract test --

    struct TestMaterializer;

    impl Materializer for TestMaterializer {
        fn dataset_name(&self) -> DatasetName {
            DatasetName::LedgerEntries
        }
        fn parser_version(&self) -> i32 {
            1
        }
        fn parser_hash(&self) -> &str {
            "sha256:test_hash"
        }
        fn chain_family(&self) -> ChainFamily {
            ChainFamily::Solana
        }
        fn descriptor(&self) -> DatasetDescriptor {
            DatasetDescriptor {
                name: self.dataset_name(),
                description: "Test ledger entries".to_string(),
                source_bronze_tables: vec!["raw_transactions".to_string()],
                chain_families: vec![self.chain_family()],
            }
        }
    }

    #[test]
    fn materializer_trait_contract() {
        let m = TestMaterializer;
        assert_eq!(m.dataset_name(), DatasetName::LedgerEntries);
        assert_eq!(m.parser_version(), 1);
        assert_eq!(m.parser_hash(), "sha256:test_hash");
        assert_eq!(m.chain_family(), ChainFamily::Solana);

        let desc = m.descriptor();
        assert!(desc.validate().is_ok());
        assert_eq!(desc.name, m.dataset_name());
    }

    // -- TokenTransfer tests --

    #[test]
    fn token_transfer_serde_roundtrip() {
        use bigdecimal::BigDecimal;
        use std::str::FromStr;

        let tt = TokenTransfer {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: Some(uuid::Uuid::new_v4()),
            network: "solana-mainnet".to_string(),
            token_address: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string(),
            token_symbol: Some("USDC".to_string()),
            from_address: "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string(),
            to_address: "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM".to_string(),
            amount: BigDecimal::from_str("1000.50").unwrap(),
            decimals: 6,
            transfer_index: 0,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&tt).unwrap();
        let back: TokenTransfer = serde_json::from_str(&json).unwrap();
        assert_eq!(back.network, "solana-mainnet");
        assert_eq!(back.token_symbol, Some("USDC".to_string()));
        assert_eq!(back.decimals, 6);
        assert_eq!(back.amount, BigDecimal::from_str("1000.50").unwrap());
    }

    #[test]
    fn token_transfer_no_wallet_specific_fields() {
        let tt = TokenTransfer {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: None,
            network: "ethereum-mainnet".to_string(),
            token_address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
            token_symbol: Some("USDC".to_string()),
            from_address: "0x1111111111111111111111111111111111111111".to_string(),
            to_address: "0x2222222222222222222222222222222222222222".to_string(),
            amount: BigDecimal::from(100),
            decimals: 6,
            transfer_index: 0,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&tt).unwrap();
        assert!(
            !json.contains("wallet_address"),
            "TokenTransfer must not have wallet_address"
        );
        assert!(
            !json.contains("user_id"),
            "TokenTransfer must not have user_id"
        );
    }

    // -- DecodedEvent tests --

    #[test]
    fn decoded_event_serde_roundtrip() {
        let de = DecodedEvent {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: Some(uuid::Uuid::new_v4()),
            network: "ethereum-mainnet".to_string(),
            program_or_contract: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string(),
            event_signature: Some(
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".to_string(),
            ),
            event_name: Some("Transfer".to_string()),
            log_index: 3,
            decoded_fields: serde_json::json!({
                "from": "0x1111111111111111111111111111111111111111",
                "to": "0x2222222222222222222222222222222222222222",
                "value": "1000000"
            }),
            raw_fields: serde_json::json!({
                "topics": ["0xddf252ad"],
                "data": "0x00000f4240"
            }),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&de).unwrap();
        let back: DecodedEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.network, "ethereum-mainnet");
        assert_eq!(back.event_name, Some("Transfer".to_string()));
        assert_eq!(back.log_index, 3);
        assert_eq!(
            back.program_or_contract,
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
    }

    #[test]
    fn decoded_event_no_wallet_specific_fields() {
        let de = DecodedEvent {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: None,
            network: "solana-mainnet".to_string(),
            program_or_contract: "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string(),
            event_signature: None,
            event_name: None,
            log_index: 0,
            decoded_fields: serde_json::json!({}),
            raw_fields: serde_json::json!({}),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&de).unwrap();
        assert!(
            !json.contains("wallet_address"),
            "DecodedEvent must not have wallet_address"
        );
        assert!(
            !json.contains("user_id"),
            "DecodedEvent must not have user_id"
        );
    }

    // -- NativeBalanceDelta tests --

    #[test]
    fn native_balance_delta_serde_roundtrip() {
        use bigdecimal::BigDecimal;
        use std::str::FromStr;

        let nbd = NativeBalanceDelta {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: Some(uuid::Uuid::new_v4()),
            network: "solana-mainnet".to_string(),
            account_address: "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string(),
            native_token: "SOL".to_string(),
            pre_balance: BigDecimal::from_str("10.5").unwrap(),
            post_balance: BigDecimal::from_str("9.5").unwrap(),
            delta: BigDecimal::from_str("-1.0").unwrap(),
            is_fee_payer: true,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&nbd).unwrap();
        let back: NativeBalanceDelta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.network, "solana-mainnet");
        assert_eq!(back.native_token, "SOL");
        assert!(back.is_fee_payer);
        assert_eq!(back.delta, BigDecimal::from_str("-1.0").unwrap());
    }

    #[test]
    fn native_balance_delta_no_wallet_specific_fields() {
        let nbd = NativeBalanceDelta {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: None,
            network: "ethereum-mainnet".to_string(),
            account_address: "0x1111111111111111111111111111111111111111".to_string(),
            native_token: "ETH".to_string(),
            pre_balance: BigDecimal::from(100),
            post_balance: BigDecimal::from(99),
            delta: BigDecimal::from(-1),
            is_fee_payer: false,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&nbd).unwrap();
        assert!(
            !json.contains("wallet_address"),
            "NativeBalanceDelta must not have wallet_address"
        );
        assert!(
            !json.contains("user_id"),
            "NativeBalanceDelta must not have user_id"
        );
    }

    #[test]
    fn native_balance_delta_zero_delta() {
        let nbd = NativeBalanceDelta {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: None,
            network: "solana-mainnet".to_string(),
            account_address: "some_address".to_string(),
            native_token: "SOL".to_string(),
            pre_balance: BigDecimal::from(5),
            post_balance: BigDecimal::from(5),
            delta: BigDecimal::from(0),
            is_fee_payer: false,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        assert_eq!(nbd.delta, BigDecimal::from(0));
    }

    // -- HlFillRecord tests (P3-W4) --

    #[test]
    fn hl_fill_record_serde_roundtrip() {
        use bigdecimal::BigDecimal;
        use std::str::FromStr;

        let fill = HlFillRecord {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: Some(uuid::Uuid::new_v4()),
            network: "hypercore-mainnet".to_string(),
            coin: "ETH".to_string(),
            side: "B".to_string(),
            price: BigDecimal::from_str("3500.0").unwrap(),
            size: BigDecimal::from_str("2.0").unwrap(),
            direction: Some("Open Long".to_string()),
            closed_pnl: Some(BigDecimal::from(0)),
            fee: Some(BigDecimal::from_str("3.50").unwrap()),
            fee_token: Some("USDC".to_string()),
            fill_time: 1700000000000,
            order_id: Some(12345),
            trade_id: Some(67890),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&fill).unwrap();
        let back: HlFillRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.network, "hypercore-mainnet");
        assert_eq!(back.coin, "ETH");
        assert_eq!(back.side, "B");
        assert_eq!(back.price, BigDecimal::from_str("3500.0").unwrap());
        assert_eq!(back.size, BigDecimal::from_str("2.0").unwrap());
        assert_eq!(back.direction, Some("Open Long".to_string()));
        assert_eq!(back.fill_time, 1700000000000);
    }

    #[test]
    fn hl_fill_record_no_wallet_specific_fields() {
        let fill = HlFillRecord {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: None,
            network: "hypercore-mainnet".to_string(),
            coin: "BTC".to_string(),
            side: "A".to_string(),
            price: BigDecimal::from(42000),
            size: BigDecimal::from(1),
            direction: None,
            closed_pnl: None,
            fee: None,
            fee_token: None,
            fill_time: 1700000000000,
            order_id: None,
            trade_id: None,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&fill).unwrap();
        assert!(
            !json.contains("wallet_address"),
            "HlFillRecord must not have wallet_address"
        );
        assert!(
            !json.contains("user_id"),
            "HlFillRecord must not have user_id"
        );
    }

    // -- HlFundingPayment tests (P3-W4) --

    #[test]
    fn hl_funding_payment_serde_roundtrip() {
        use bigdecimal::BigDecimal;
        use std::str::FromStr;

        let fp = HlFundingPayment {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: Some(uuid::Uuid::new_v4()),
            network: "hypercore-mainnet".to_string(),
            coin: "ETH".to_string(),
            amount: BigDecimal::from_str("-2.50").unwrap(),
            funding_rate: Some(BigDecimal::from_str("0.0001").unwrap()),
            payment_time: 1700000000000,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&fp).unwrap();
        let back: HlFundingPayment = serde_json::from_str(&json).unwrap();
        assert_eq!(back.network, "hypercore-mainnet");
        assert_eq!(back.coin, "ETH");
        assert_eq!(back.amount, BigDecimal::from_str("-2.50").unwrap());
        assert_eq!(back.payment_time, 1700000000000);
    }

    #[test]
    fn hl_funding_payment_no_wallet_specific_fields() {
        let fp = HlFundingPayment {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: None,
            network: "hypercore-mainnet".to_string(),
            coin: "BTC".to_string(),
            amount: BigDecimal::from(5),
            funding_rate: None,
            payment_time: 1700000000000,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&fp).unwrap();
        assert!(
            !json.contains("wallet_address"),
            "HlFundingPayment must not have wallet_address"
        );
        assert!(
            !json.contains("user_id"),
            "HlFundingPayment must not have user_id"
        );
    }

    // -- HlPositionChange tests (P3-W4) --

    #[test]
    fn hl_position_change_serde_roundtrip() {
        use bigdecimal::BigDecimal;
        use std::str::FromStr;

        let pc = HlPositionChange {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: Some(uuid::Uuid::new_v4()),
            network: "hypercore-mainnet".to_string(),
            coin: "ETH".to_string(),
            side: "B".to_string(),
            size_delta: BigDecimal::from_str("2.0").unwrap(),
            price: BigDecimal::from_str("3500.0").unwrap(),
            direction: Some("Open Long".to_string()),
            source_event: "fill".to_string(),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&pc).unwrap();
        let back: HlPositionChange = serde_json::from_str(&json).unwrap();
        assert_eq!(back.network, "hypercore-mainnet");
        assert_eq!(back.coin, "ETH");
        assert_eq!(back.side, "B");
        assert_eq!(back.size_delta, BigDecimal::from_str("2.0").unwrap());
        assert_eq!(back.source_event, "fill");
    }

    #[test]
    fn hl_position_change_no_wallet_specific_fields() {
        let pc = HlPositionChange {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: None,
            network: "hypercore-mainnet".to_string(),
            coin: "BTC".to_string(),
            side: "A".to_string(),
            size_delta: BigDecimal::from(-1),
            price: BigDecimal::from(42000),
            direction: None,
            source_event: "liquidation".to_string(),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&pc).unwrap();
        assert!(
            !json.contains("wallet_address"),
            "HlPositionChange must not have wallet_address"
        );
        assert!(
            !json.contains("user_id"),
            "HlPositionChange must not have user_id"
        );
    }

    // -- SinkType tests (P4-W3) --

    #[test]
    fn sink_type_serde_roundtrip() {
        for (variant, expected) in [
            (SinkType::LocalFile, "\"local_file\""),
            (SinkType::Webhook, "\"webhook\""),
            (SinkType::Database, "\"database\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected, "serialize {variant:?}");
            let back: SinkType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant, "deserialize {expected}");
        }
    }

    #[test]
    fn sink_type_from_str() {
        assert_eq!(
            SinkType::from_str("local_file").unwrap(),
            SinkType::LocalFile
        );
        assert_eq!(SinkType::from_str("webhook").unwrap(), SinkType::Webhook);
        assert_eq!(SinkType::from_str("database").unwrap(), SinkType::Database);
        assert!(SinkType::from_str("object_storage").is_err());
        assert!(SinkType::from_str("s3").is_err());
    }

    #[test]
    fn sink_type_display() {
        assert_eq!(SinkType::LocalFile.to_string(), "local_file");
        assert_eq!(SinkType::Webhook.to_string(), "webhook");
        assert_eq!(SinkType::Database.to_string(), "database");
    }

    // -- SinkConfig tests (P4-W3) --

    #[test]
    fn sink_config_local_file_valid() {
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some("/tmp/export.jsonl".to_string()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn sink_config_local_file_missing_path() {
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: None,
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("file_path"));
    }

    #[test]
    fn sink_config_local_file_empty_path() {
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some(String::new()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("file_path"));
    }

    #[test]
    fn sink_config_webhook_valid() {
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: None,
            connection_string: None,
            table: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn sink_config_webhook_missing_url() {
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("url"));
    }

    #[test]
    fn sink_config_webhook_with_headers() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-API-Key".to_string(), "secret".to_string());
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: Some(headers),
            connection_string: None,
            table: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn sink_config_database_valid() {
        let config = SinkConfig {
            sink_type: SinkType::Database,
            file_path: None,
            url: None,
            headers: None,
            connection_string: Some("postgresql://localhost/exports".to_string()),
            table: Some("export_data".to_string()),
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn sink_config_database_missing_connection_string() {
        let config = SinkConfig {
            sink_type: SinkType::Database,
            file_path: None,
            url: None,
            headers: None,
            connection_string: None,
            table: Some("export_data".to_string()),
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("connection_string"));
    }

    #[test]
    fn sink_config_database_missing_table() {
        let config = SinkConfig {
            sink_type: SinkType::Database,
            file_path: None,
            url: None,
            headers: None,
            connection_string: Some("postgresql://localhost/exports".to_string()),
            table: None,
        };
        let err = config.validate().unwrap_err();
        assert!(err.contains("table"));
    }

    #[test]
    fn sink_config_serde_roundtrip() {
        let config = SinkConfig {
            sink_type: SinkType::Webhook,
            file_path: None,
            url: Some("https://example.com/hook".to_string()),
            headers: Some({
                let mut h = std::collections::HashMap::new();
                h.insert("Authorization".to_string(), "Bearer tok".to_string());
                h
            }),
            connection_string: None,
            table: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SinkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sink_type, SinkType::Webhook);
        assert_eq!(back.url, Some("https://example.com/hook".to_string()));
        assert!(back.headers.is_some());
    }

    #[test]
    fn sink_config_local_file_serde_roundtrip() {
        let config = SinkConfig {
            sink_type: SinkType::LocalFile,
            file_path: Some("/tmp/export.csv".to_string()),
            url: None,
            headers: None,
            connection_string: None,
            table: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SinkConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sink_type, SinkType::LocalFile);
        assert_eq!(back.file_path, Some("/tmp/export.csv".to_string()));
    }

    // -- DeliveryMetadata tests --

    #[test]
    fn delivery_metadata_serde_roundtrip() {
        let meta = DeliveryMetadata {
            job_id: uuid::Uuid::new_v4(),
            dataset: "token_transfers".to_string(),
            format: "jsonl".to_string(),
            record_count: 42,
            dataset_version_id: None,
            completeness_status: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: DeliveryMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dataset, "token_transfers");
        assert_eq!(back.record_count, 42);
        // Optional fields absent when None
        assert!(back.dataset_version_id.is_none());
        assert!(back.completeness_status.is_none());
    }

    #[test]
    fn delivery_metadata_with_provenance_fields() {
        let version_id = uuid::Uuid::new_v4();
        let meta = DeliveryMetadata {
            job_id: uuid::Uuid::new_v4(),
            dataset: "hl_fills".to_string(),
            format: "csv".to_string(),
            record_count: 100,
            dataset_version_id: Some(version_id),
            completeness_status: Some("complete".to_string()),
        };
        let json = serde_json::to_value(&meta).unwrap();
        assert_eq!(json["dataset_version_id"], version_id.to_string());
        assert_eq!(json["completeness_status"], "complete");
    }

    #[test]
    fn delivery_metadata_skip_none_provenance_fields() {
        let meta = DeliveryMetadata {
            job_id: uuid::Uuid::new_v4(),
            dataset: "token_transfers".to_string(),
            format: "jsonl".to_string(),
            record_count: 0,
            dataset_version_id: None,
            completeness_status: None,
        };
        let json = serde_json::to_value(&meta).unwrap();
        assert!(json.get("dataset_version_id").is_none());
        assert!(json.get("completeness_status").is_none());
    }

    // -- DeliveryReceipt tests --

    #[test]
    fn delivery_receipt_serde_roundtrip() {
        let receipt = DeliveryReceipt {
            sink_type: SinkType::LocalFile,
            destination: "/tmp/export.jsonl".to_string(),
            bytes_written: 1024,
            delivered_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&receipt).unwrap();
        let back: DeliveryReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sink_type, SinkType::LocalFile);
        assert_eq!(back.destination, "/tmp/export.jsonl");
        assert_eq!(back.bytes_written, 1024);
    }

    // -- WalletLedgerRecord tests (P5-W1) --

    #[test]
    fn wallet_ledger_record_serde_roundtrip() {
        let record = WalletLedgerRecord {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: Some(uuid::Uuid::new_v4()),
            wallet_address: "0xWallet".to_string(),
            network: "solana-mainnet".to_string(),
            tx_hash: "abc123".to_string(),
            timestamp: 1700000000,
            entry_type: "transfer".to_string(),
            asset_symbol: "USDC".to_string(),
            amount: BigDecimal::from_str("100.5").unwrap(),
            counterparty_address: Some("0xCounterparty".to_string()),
            fee_amount: Some(BigDecimal::from_str("0.005").unwrap()),
            fee_asset: Some("SOL".to_string()),
            cost_basis: None,
            proceeds: None,
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: WalletLedgerRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.wallet_address, "0xWallet");
        assert_eq!(back.entry_type, "transfer");
        assert_eq!(
            back.counterparty_address,
            Some("0xCounterparty".to_string())
        );
        assert_eq!(back.amount, BigDecimal::from_str("100.5").unwrap());
    }

    #[test]
    fn wallet_ledger_record_nullable_fields() {
        let record = WalletLedgerRecord {
            id: uuid::Uuid::new_v4(),
            raw_transaction_id: None,
            wallet_address: "0xWallet".to_string(),
            network: "ethereum-mainnet".to_string(),
            tx_hash: "0xdeadbeef".to_string(),
            timestamp: 1700000000,
            entry_type: "fee".to_string(),
            asset_symbol: "ETH".to_string(),
            amount: BigDecimal::from_str("-0.001").unwrap(),
            counterparty_address: None,
            fee_amount: None,
            fee_asset: None,
            cost_basis: Some(BigDecimal::from_str("3500.0").unwrap()),
            proceeds: Some(BigDecimal::from_str("3600.0").unwrap()),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_value(&record).unwrap();
        assert!(json["counterparty_address"].is_null());
        assert_eq!(json["cost_basis"].as_str(), Some("3500.0"));
        assert_eq!(json["proceeds"].as_str(), Some("3600.0"));
    }

    // -- BalanceSnapshot tests (P5-W1) --

    #[test]
    fn balance_snapshot_serde_roundtrip() {
        let snap = BalanceSnapshot {
            id: uuid::Uuid::new_v4(),
            wallet_address: "0xWallet".to_string(),
            asset_symbol: "SOL".to_string(),
            network: "solana-mainnet".to_string(),
            timestamp: 1700000000,
            balance: BigDecimal::from_str("42.5").unwrap(),
            tx_hash: "abc123".to_string(),
            dataset_version_id: None,
            created_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: BalanceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.wallet_address, "0xWallet");
        assert_eq!(back.asset_symbol, "SOL");
        assert_eq!(back.balance, BigDecimal::from_str("42.5").unwrap());
    }

    // -- ForensicsActivity tests (P5-W1) --

    #[test]
    fn forensics_activity_serde_roundtrip() {
        let activity = ForensicsActivity {
            wallet_address: "0xWallet".to_string(),
            top_counterparties: vec![CounterpartySummary {
                address: "0xCounterparty".to_string(),
                interaction_count: 5,
                total_inflow: BigDecimal::from(500),
                total_outflow: BigDecimal::from(200),
                networks: vec!["solana-mainnet".to_string()],
            }],
            network_activity: vec![NetworkActivity {
                network: "solana-mainnet".to_string(),
                entry_count: 10,
                unique_assets: 3,
                unique_counterparties: 2,
            }],
            type_breakdown: vec![TypeBreakdown {
                entry_type: "transfer".to_string(),
                count: 7,
                total_amount: BigDecimal::from(1000),
            }],
            total_entries: 10,
        };
        let json = serde_json::to_string(&activity).unwrap();
        let back: ForensicsActivity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.wallet_address, "0xWallet");
        assert_eq!(back.top_counterparties.len(), 1);
        assert_eq!(back.top_counterparties[0].interaction_count, 5);
        assert_eq!(back.network_activity.len(), 1);
        assert_eq!(back.type_breakdown.len(), 1);
        assert_eq!(back.total_entries, 10);
    }

    // -- P5-W3: Protocol / TVL record and analytics serde tests --

    #[test]
    fn protocol_event_serde_roundtrip() {
        let pe = ProtocolEvent {
            id: Uuid::nil(),
            network: "ethereum-mainnet".to_string(),
            protocol_address: "0xUniswapV3".to_string(),
            protocol_name: Some("Uniswap V3".to_string()),
            event_type: "swap".to_string(),
            event_details: serde_json::json!({"amount0": "100", "amount1": "-50"}),
            pool_address: Some("0xPool123".to_string()),
            raw_event_id: Some(Uuid::nil()),
            timestamp: 1700000000,
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        };
        let json = serde_json::to_string(&pe).unwrap();
        let back: ProtocolEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.protocol_address, "0xUniswapV3");
        assert_eq!(back.event_type, "swap");
        assert_eq!(back.pool_address.as_deref(), Some("0xPool123"));
    }

    #[test]
    fn pool_snapshot_serde_roundtrip() {
        let ps = PoolSnapshot {
            id: Uuid::nil(),
            network: "ethereum-mainnet".to_string(),
            pool_address: "0xPool123".to_string(),
            protocol_address: "0xUniswapV3".to_string(),
            protocol_name: Some("Uniswap V3".to_string()),
            token0_address: "0xTokenA".to_string(),
            token0_symbol: Some("WETH".to_string()),
            token1_address: "0xTokenB".to_string(),
            token1_symbol: Some("USDC".to_string()),
            reserve0: BigDecimal::from_str("1000.5").unwrap(),
            reserve1: BigDecimal::from_str("2000000").unwrap(),
            tvl_usd: Some(BigDecimal::from(4000000)),
            snapshot_timestamp: 1700000000,
            block_number: Some(18000000),
            dataset_version_id: None,
            created_at: chrono::DateTime::from_timestamp(1700000000, 0).unwrap(),
        };
        let json = serde_json::to_string(&ps).unwrap();
        let back: PoolSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pool_address, "0xPool123");
        assert_eq!(back.reserve0, BigDecimal::from_str("1000.5").unwrap());
        assert_eq!(back.tvl_usd, Some(BigDecimal::from(4000000)));
    }

    #[test]
    fn protocol_activity_serde_roundtrip() {
        let pa = ProtocolActivity {
            protocol_address: "0xUniswapV3".to_string(),
            event_counts_by_type: vec![
                EventTypeCount {
                    event_type: "swap".to_string(),
                    count: 100,
                },
                EventTypeCount {
                    event_type: "mint".to_string(),
                    count: 10,
                },
            ],
            unique_participants: 50,
            total_events: 110,
            time_start: Some(1700000000),
            time_end: Some(1700100000),
        };
        let json = serde_json::to_string(&pa).unwrap();
        let back: ProtocolActivity = serde_json::from_str(&json).unwrap();
        assert_eq!(back.protocol_address, "0xUniswapV3");
        assert_eq!(back.event_counts_by_type.len(), 2);
        assert_eq!(back.unique_participants, 50);
        assert_eq!(back.total_events, 110);
    }

    #[test]
    fn tvl_analytics_serde_roundtrip() {
        let tvl = TvlAnalytics {
            pools: vec![PoolTvlSummary {
                pool_address: "0xPool".to_string(),
                protocol_address: "0xProto".to_string(),
                token0_symbol: Some("WETH".to_string()),
                token1_symbol: Some("USDC".to_string()),
                reserve0: BigDecimal::from(1000),
                reserve1: BigDecimal::from(2000000),
                tvl_usd: Some(BigDecimal::from(4000000)),
                snapshot_timestamp: 1700000000,
            }],
            total_tvl: Some(BigDecimal::from(4000000)),
            protocols: vec![ProtocolTvlSummary {
                protocol_address: "0xProto".to_string(),
                protocol_name: Some("Uniswap".to_string()),
                pool_count: 1,
                total_tvl: Some(BigDecimal::from(4000000)),
            }],
        };
        let json = serde_json::to_string(&tvl).unwrap();
        let back: TvlAnalytics = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pools.len(), 1);
        assert_eq!(back.protocols.len(), 1);
        assert_eq!(back.total_tvl, Some(BigDecimal::from(4000000)));
    }

    // -----------------------------------------------------------------------
    // Dataset Registry tests
    // -----------------------------------------------------------------------

    #[test]
    fn physical_table_mapping() {
        // Datasets where canonical != physical
        assert_eq!(DatasetName::HlFills.physical_table(), "hl_fill_records");
        assert_eq!(
            DatasetName::HlFunding.physical_table(),
            "hl_funding_payments"
        );
        assert_eq!(
            DatasetName::Positions.physical_table(),
            "hl_position_changes"
        );

        // Datasets where canonical == physical
        assert_eq!(
            DatasetName::TokenTransfers.physical_table(),
            "token_transfers"
        );
        assert_eq!(
            DatasetName::NativeBalanceDeltas.physical_table(),
            "native_balance_deltas"
        );
        assert_eq!(
            DatasetName::DecodedEvents.physical_table(),
            "decoded_events"
        );
        assert_eq!(
            DatasetName::LedgerEntries.physical_table(),
            "ledger_entries"
        );
        assert_eq!(DatasetName::WalletLedger.physical_table(), "wallet_ledger");
        assert_eq!(
            DatasetName::BalanceHistory.physical_table(),
            "balance_history"
        );
        assert_eq!(DatasetName::HlPnlSummary.physical_table(), "hl_pnl_summary");
        assert_eq!(
            DatasetName::HlTradeHistory.physical_table(),
            "hl_trade_history"
        );
        assert_eq!(
            DatasetName::ProtocolEvents.physical_table(),
            "protocol_events"
        );
        assert_eq!(
            DatasetName::PoolSnapshots.physical_table(),
            "pool_snapshots"
        );
    }

    #[test]
    fn from_physical_table_roundtrip() {
        // Every dataset can be recovered from its physical table name.
        for ds in DatasetName::all() {
            let recovered = DatasetName::from_physical_table(ds.physical_table());
            assert_eq!(
                recovered,
                Some(*ds),
                "from_physical_table({}) should return {:?}",
                ds.physical_table(),
                ds
            );
        }
    }

    #[test]
    fn from_physical_table_unknown_returns_none() {
        assert_eq!(DatasetName::from_physical_table("nonexistent_table"), None);
        assert_eq!(DatasetName::from_physical_table(""), None);
    }

    #[test]
    fn from_physical_table_canonical_names_also_work() {
        // Canonical names should also resolve (they are the same as physical
        // for most datasets, and for the three HL datasets they are the
        // canonical form which FromStr handles).
        assert_eq!(
            DatasetName::from_physical_table("token_transfers"),
            Some(DatasetName::TokenTransfers)
        );
        assert_eq!(
            DatasetName::from_physical_table("wallet_ledger"),
            Some(DatasetName::WalletLedger)
        );
    }

    #[test]
    fn tier_classification() {
        // Silver datasets
        assert_eq!(DatasetName::LedgerEntries.tier(), DatasetTier::Silver);
        assert_eq!(DatasetName::TokenTransfers.tier(), DatasetTier::Silver);
        assert_eq!(DatasetName::NativeBalanceDeltas.tier(), DatasetTier::Silver);
        assert_eq!(DatasetName::DecodedEvents.tier(), DatasetTier::Silver);
        assert_eq!(DatasetName::HlFills.tier(), DatasetTier::Silver);
        assert_eq!(DatasetName::HlFunding.tier(), DatasetTier::Silver);
        assert_eq!(DatasetName::Positions.tier(), DatasetTier::Silver);

        // Gold datasets
        assert_eq!(DatasetName::WalletLedger.tier(), DatasetTier::Gold);
        assert_eq!(DatasetName::BalanceHistory.tier(), DatasetTier::Gold);
        assert_eq!(DatasetName::HlPnlSummary.tier(), DatasetTier::Gold);
        assert_eq!(DatasetName::HlTradeHistory.tier(), DatasetTier::Gold);
        assert_eq!(DatasetName::ProtocolEvents.tier(), DatasetTier::Gold);
        assert_eq!(DatasetName::PoolSnapshots.tier(), DatasetTier::Gold);
    }

    #[test]
    fn silver_gold_partitions_cover_all_datasets() {
        let mut all_partitioned: Vec<DatasetName> = Vec::new();
        all_partitioned.extend_from_slice(DatasetName::silver());
        all_partitioned.extend_from_slice(DatasetName::gold());
        all_partitioned.sort_by_key(|d| d.as_sql_str());

        let mut all_datasets: Vec<DatasetName> = DatasetName::all().to_vec();
        all_datasets.sort_by_key(|d| d.as_sql_str());

        assert_eq!(
            all_partitioned, all_datasets,
            "silver() + gold() must cover all datasets exactly once"
        );
    }

    #[test]
    fn tier_matches_partition() {
        for ds in DatasetName::silver() {
            assert_eq!(
                ds.tier(),
                DatasetTier::Silver,
                "{ds:?} is in silver() but tier() returns {:?}",
                ds.tier()
            );
        }
        for ds in DatasetName::gold() {
            assert_eq!(
                ds.tier(),
                DatasetTier::Gold,
                "{ds:?} is in gold() but tier() returns {:?}",
                ds.tier()
            );
        }
    }

    #[test]
    fn chain_families_non_empty() {
        for ds in DatasetName::all() {
            assert!(
                !ds.chain_families().is_empty(),
                "{ds:?} must support at least one chain family"
            );
        }
    }

    #[test]
    fn hl_datasets_support_hyperliquid() {
        for ds in &[
            DatasetName::HlFills,
            DatasetName::HlFunding,
            DatasetName::Positions,
            DatasetName::HlPnlSummary,
            DatasetName::HlTradeHistory,
        ] {
            assert!(
                ds.chain_families().contains(&ChainFamily::Hyperliquid),
                "{ds:?} should support Hyperliquid"
            );
        }
    }

    #[test]
    fn registry_resolve_canonical_names() {
        for ds in DatasetName::all() {
            let resolved = DatasetRegistry::resolve(ds.as_sql_str());
            assert_eq!(
                resolved,
                Some(*ds),
                "resolve({}) should return {:?}",
                ds.as_sql_str(),
                ds
            );
        }
    }

    #[test]
    fn registry_resolve_physical_names() {
        // Physical names that differ from canonical should still resolve.
        assert_eq!(
            DatasetRegistry::resolve("hl_fill_records"),
            Some(DatasetName::HlFills)
        );
        assert_eq!(
            DatasetRegistry::resolve("hl_funding_payments"),
            Some(DatasetName::HlFunding)
        );
        assert_eq!(
            DatasetRegistry::resolve("hl_position_changes"),
            Some(DatasetName::Positions)
        );
    }

    #[test]
    fn registry_resolve_unknown_returns_none() {
        assert_eq!(DatasetRegistry::resolve("unknown"), None);
        assert_eq!(DatasetRegistry::resolve(""), None);
    }

    #[test]
    fn registry_queryable_contains_all_non_legacy_datasets() {
        let queryable = DatasetRegistry::queryable();
        // All 12 non-legacy datasets should be queryable.
        assert_eq!(queryable.len(), 12);
        // LedgerEntries should not be in the queryable list.
        assert!(
            !queryable.contains(&DatasetName::LedgerEntries),
            "LedgerEntries served via legacy endpoint, not dataset query"
        );
    }

    #[test]
    fn registry_exportable_contains_all_non_legacy_datasets() {
        let exportable = DatasetRegistry::exportable();
        assert_eq!(exportable.len(), 12);
        assert!(
            !exportable.contains(&DatasetName::LedgerEntries),
            "LedgerEntries served via legacy endpoint, not dataset export"
        );
    }

    #[test]
    fn registry_is_queryable() {
        assert!(DatasetRegistry::is_queryable("token_transfers"));
        assert!(DatasetRegistry::is_queryable("hl_fills"));
        assert!(DatasetRegistry::is_queryable("wallet_ledger"));
        assert!(!DatasetRegistry::is_queryable("ledger_entries"));
        assert!(!DatasetRegistry::is_queryable("nonexistent"));
    }

    #[test]
    fn registry_is_exportable() {
        assert!(DatasetRegistry::is_exportable("token_transfers"));
        assert!(DatasetRegistry::is_exportable("pool_snapshots"));
        assert!(!DatasetRegistry::is_exportable("ledger_entries"));
        assert!(!DatasetRegistry::is_exportable("nonexistent"));
    }

    #[test]
    fn chain_family_for_network_lookup() {
        // Solana family
        assert_eq!(
            DatasetRegistry::chain_family_for_network("solana-mainnet"),
            Some(ChainFamily::Solana)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("solana-devnet"),
            Some(ChainFamily::Solana)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("solana-testnet"),
            Some(ChainFamily::Solana)
        );

        // EVM family
        assert_eq!(
            DatasetRegistry::chain_family_for_network("ethereum-mainnet"),
            Some(ChainFamily::Evm)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("ethereum-sepolia"),
            Some(ChainFamily::Evm)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("base-mainnet"),
            Some(ChainFamily::Evm)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("base-sepolia"),
            Some(ChainFamily::Evm)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("arbitrum-mainnet"),
            Some(ChainFamily::Evm)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("arbitrum-sepolia"),
            Some(ChainFamily::Evm)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("hyperevm-mainnet"),
            Some(ChainFamily::Evm)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("hyperevm-testnet"),
            Some(ChainFamily::Evm)
        );

        // Hyperliquid family
        assert_eq!(
            DatasetRegistry::chain_family_for_network("hypercore-mainnet"),
            Some(ChainFamily::Hyperliquid)
        );
        assert_eq!(
            DatasetRegistry::chain_family_for_network("hypercore-testnet"),
            Some(ChainFamily::Hyperliquid)
        );

        // Unknown
        assert_eq!(
            DatasetRegistry::chain_family_for_network("unknown-network"),
            None
        );
        assert_eq!(DatasetRegistry::chain_family_for_network(""), None);
    }

    #[test]
    fn silver_datasets_for_family_coverage() {
        let solana = DatasetRegistry::silver_datasets_for_family(ChainFamily::Solana);
        assert!(solana.contains(&DatasetName::TokenTransfers));
        assert!(solana.contains(&DatasetName::NativeBalanceDeltas));
        assert!(solana.contains(&DatasetName::DecodedEvents));
        assert!(!solana.contains(&DatasetName::HlFills));

        let evm = DatasetRegistry::silver_datasets_for_family(ChainFamily::Evm);
        assert!(evm.contains(&DatasetName::TokenTransfers));
        assert!(evm.contains(&DatasetName::DecodedEvents));
        // NativeBalanceDeltas not yet materializable for EVM (needs trace infra)
        assert!(!evm.contains(&DatasetName::NativeBalanceDeltas));
        assert!(!evm.contains(&DatasetName::HlFills));

        let hl = DatasetRegistry::silver_datasets_for_family(ChainFamily::Hyperliquid);
        assert!(hl.contains(&DatasetName::TokenTransfers));
        assert!(hl.contains(&DatasetName::NativeBalanceDeltas));
        assert!(hl.contains(&DatasetName::HlFills));
        assert!(hl.contains(&DatasetName::HlFunding));
        assert!(hl.contains(&DatasetName::Positions));
        assert!(!hl.contains(&DatasetName::DecodedEvents));
    }

    #[test]
    fn registry_silver_datasets_for_network() {
        // Solana networks
        let solana = DatasetRegistry::silver_datasets_for_network("solana-mainnet");
        assert!(solana.contains(&DatasetName::TokenTransfers));
        assert!(solana.contains(&DatasetName::NativeBalanceDeltas));
        assert!(solana.contains(&DatasetName::DecodedEvents));

        // All EVM networks produce the same datasets
        for network in &[
            "ethereum-mainnet",
            "ethereum-sepolia",
            "base-mainnet",
            "base-sepolia",
            "arbitrum-mainnet",
            "arbitrum-sepolia",
            "hyperevm-mainnet",
            "hyperevm-testnet",
        ] {
            let evm = DatasetRegistry::silver_datasets_for_network(network);
            assert!(
                evm.contains(&DatasetName::TokenTransfers),
                "{network} missing token_transfers"
            );
            assert!(
                evm.contains(&DatasetName::DecodedEvents),
                "{network} missing decoded_events"
            );
            assert!(
                !evm.contains(&DatasetName::NativeBalanceDeltas),
                "{network} should not have native_balance_deltas (no trace materializer)"
            );
        }

        // Hyperliquid networks
        let hl = DatasetRegistry::silver_datasets_for_network("hypercore-mainnet");
        assert!(hl.contains(&DatasetName::TokenTransfers));
        assert!(hl.contains(&DatasetName::NativeBalanceDeltas));
        assert!(hl.contains(&DatasetName::HlFills));
        assert!(hl.contains(&DatasetName::HlFunding));
        assert!(hl.contains(&DatasetName::Positions));

        let hl_test = DatasetRegistry::silver_datasets_for_network("hypercore-testnet");
        assert_eq!(hl.len(), hl_test.len());

        // Unknown network returns empty
        let unknown = DatasetRegistry::silver_datasets_for_network("unknown-network");
        assert!(unknown.is_empty());
    }

    #[test]
    fn registry_silver_materializable() {
        let mat = DatasetRegistry::silver_materializable();
        assert_eq!(mat.len(), 6);
        // Should not include LedgerEntries (legacy Silver) or any Gold datasets.
        assert!(!mat.contains(&DatasetName::LedgerEntries));
        assert!(!mat.contains(&DatasetName::WalletLedger));
    }

    #[test]
    fn dataset_tier_serde_roundtrip() {
        for tier in &[DatasetTier::Bronze, DatasetTier::Silver, DatasetTier::Gold] {
            let json = serde_json::to_string(tier).unwrap();
            let back: DatasetTier = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, tier);
        }
    }

    #[test]
    fn dataset_tier_display() {
        assert_eq!(DatasetTier::Bronze.to_string(), "bronze");
        assert_eq!(DatasetTier::Silver.to_string(), "silver");
        assert_eq!(DatasetTier::Gold.to_string(), "gold");
    }
}
