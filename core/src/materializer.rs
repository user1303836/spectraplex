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
}
