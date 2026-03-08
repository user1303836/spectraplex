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

/// Canonical Silver dataset identifiers.
///
/// These correspond to the seven Silver datasets defined in
/// V2_ARCHITECTURE_RFC Section 3.5. Each variant maps to a normalized
/// table or logical dataset produced from Bronze raw data.
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
}

impl DatasetName {
    /// Returns the SQL-compatible string for this dataset name.
    pub fn as_sql_str(&self) -> &'static str {
        match self {
            DatasetName::LedgerEntries => "ledger_entries",
            DatasetName::TokenTransfers => "token_transfers",
            DatasetName::NativeBalanceDeltas => "native_balance_deltas",
            DatasetName::DecodedEvents => "decoded_events",
            DatasetName::HlFills => "hl_fills",
            DatasetName::HlFunding => "hl_funding",
            DatasetName::Positions => "positions",
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
        ]
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use strum::IntoEnumIterator;

    #[test]
    fn dataset_name_count_is_seven() {
        assert_eq!(DatasetName::all().len(), 7);
        assert_eq!(DatasetName::iter().count(), 7);
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
        ];
        assert_eq!(cases.len(), 7, "must cover all 7 datasets");
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
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: DeliveryMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.dataset, "token_transfers");
        assert_eq!(back.record_count, 42);
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
}
