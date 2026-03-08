//! V2 Connector trait and typed filter specifications.
//!
//! This module introduces a filter-driven connector abstraction that accepts
//! typed `IndexTarget` specs instead of bare wallet addresses. It defines:
//!
//! - The `Connector` async trait with `backfill` and `stream` methods
//! - `ConnectorCapabilities` describing what a connector supports
//! - Typed filter spec structs parsed from `IndexTarget.filter_spec` JSONB
//! - Validation helpers for target-connector compatibility

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::v2::{ChainFamily, IndexTarget, IngestionBatch, RawTransaction, TargetKind, TargetMode};

// ---------------------------------------------------------------------------
// Connector capabilities
// ---------------------------------------------------------------------------

/// Describes the capabilities of a specific connector implementation.
#[derive(Debug, Clone)]
pub struct ConnectorCapabilities {
    /// Which target kinds this connector can handle.
    pub supported_target_kinds: Vec<TargetKind>,
    /// Which ingestion modes this connector supports.
    pub supported_modes: Vec<TargetMode>,
    /// The chain family this connector serves.
    pub chain_family: ChainFamily,
}

impl ConnectorCapabilities {
    /// Returns `true` if the connector supports the given target kind.
    pub fn supports_target_kind(&self, kind: TargetKind) -> bool {
        self.supported_target_kinds.contains(&kind)
    }

    /// Returns `true` if the connector supports the given mode.
    pub fn supports_mode(&self, mode: TargetMode) -> bool {
        match mode {
            TargetMode::Both => {
                self.supported_modes.contains(&TargetMode::Backfill)
                    || self.supported_modes.contains(&TargetMode::Both)
            }
            _ => self.supported_modes.contains(&mode),
        }
    }

    /// Returns `true` if the connector can service the given target.
    pub fn can_service(&self, target: &IndexTarget) -> bool {
        self.chain_family == target.chain_family && self.supports_target_kind(target.kind)
    }
}

// ---------------------------------------------------------------------------
// Connector trait
// ---------------------------------------------------------------------------

/// A filter-driven connector that accepts typed target specs for ingestion.
///
/// This replaces the wallet-only `ChainIngestor` trait. Each connector
/// implementation serves one `ChainFamily` and declares which `TargetKind`s
/// and `TargetMode`s it supports via `capabilities()`.
///
/// The control plane (orchestrator) is responsible for:
/// - routing targets to the correct connector based on capabilities
/// - coordinating backfill+stream when `mode = Both` across connectors
/// - managing checkpoints and ingestion runs
#[async_trait::async_trait]
pub trait Connector: Send + Sync {
    /// Declare what this connector can do.
    fn capabilities(&self) -> ConnectorCapabilities;

    /// Fetch historical data for a target.
    ///
    /// - `target`: the registered index target with its filter spec
    /// - `cursor`: optional checkpoint cursor (opaque JSON from a previous run)
    /// - `limit`: maximum number of raw transactions to return
    ///
    /// Returns an `IngestionBatch` containing raw transactions and an optional
    /// updated cursor for the next call.
    async fn backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch>;

    /// Open a real-time stream for a target.
    ///
    /// - `target`: the registered index target with its filter spec
    /// - `cursor`: optional checkpoint cursor to resume from
    ///
    /// Returns a channel receiver that yields raw transactions as they arrive.
    /// The background task is spawned internally; dropping the receiver stops it.
    ///
    /// Default implementation returns an error for connectors that only
    /// support backfill.
    async fn stream(
        &self,
        _target: &IndexTarget,
        _cursor: Option<&serde_json::Value>,
    ) -> anyhow::Result<mpsc::Receiver<RawTransaction>> {
        anyhow::bail!(
            "streaming not supported by this connector ({})",
            self.capabilities().chain_family
        )
    }
}

// ---------------------------------------------------------------------------
// Typed filter specs
// ---------------------------------------------------------------------------

/// Wallet-specific filter (optional narrowing beyond the address).
/// Per TARGET_MODEL.md Section 2.1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WalletFilterSpec {
    /// Restrict downstream matches to specific assets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub asset_filter: Vec<String>,
    /// Minimum amount threshold for matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_amount: Option<String>,
    /// Restrict to specific transfer directions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directions: Vec<String>,
}

/// Contract-specific filter (EVM only).
/// Per TARGET_MODEL.md Section 2.2.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContractFilterSpec {
    /// ABI event signatures to filter (e.g. "Transfer(address,address,uint256)").
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_signatures: Vec<String>,
    /// Additional topic-position filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_filters: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Program-specific filter (Solana only).
/// Per TARGET_MODEL.md Section 2.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramFilterSpec {
    /// Instruction discriminator bytes to match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instruction_discriminators: Vec<String>,
    /// Whether CPI invocations count as matches (default: true).
    #[serde(default = "default_true")]
    pub include_cpi: bool,
}

impl Default for ProgramFilterSpec {
    fn default() -> Self {
        Self {
            instruction_discriminators: Vec::new(),
            include_cpi: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Account-specific filter (primarily Solana).
/// Per TARGET_MODEL.md Section 2.4.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountFilterSpec {
    /// Restrict to write-only or read-only references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub access_types: Vec<String>,
    /// Informational: program that owns this account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub associated_program: Option<String>,
}

/// Topic-based filter (EVM only, address is null on the target).
/// Per TARGET_MODEL.md Section 2.5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicFilterSpec {
    /// Up to 4 topic positions. Each is a hex string, null, or an array of hex strings.
    pub topics: Vec<serde_json::Value>,
    /// Optional contract address narrowing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub address_filter: Vec<String>,
}

/// Market-specific filter (Hyperliquid only).
/// Per TARGET_MODEL.md Section 2.6.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketFilterSpec {
    /// Types of market data to fetch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub data_types: Vec<String>,
    /// Minimum trade size (query-time filter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size: Option<String>,
}

/// Pool-specific filter (EVM and Solana).
/// Per TARGET_MODEL.md Section 2.7.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolFilterSpec {
    /// Informational protocol name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Informational token pair.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_pair: Vec<String>,
    /// Event types to match.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_types: Vec<String>,
}

/// Protocol address entry within a protocol filter spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolAddress {
    pub address: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Protocol-level filter (composite target spanning multiple addresses).
/// Per TARGET_MODEL.md Section 2.8.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolFilterSpec {
    /// Protocol identifier name.
    pub name: String,
    /// Network → list of address entries.
    pub addresses: std::collections::HashMap<String, Vec<ProtocolAddress>>,
}

// ---------------------------------------------------------------------------
// Typed filter extraction
// ---------------------------------------------------------------------------

/// A parsed, typed representation of an `IndexTarget`'s filter spec.
#[derive(Debug, Clone)]
pub enum TypedFilterSpec {
    Wallet(WalletFilterSpec),
    Contract(ContractFilterSpec),
    Program(ProgramFilterSpec),
    Account(AccountFilterSpec),
    TopicFilter(TopicFilterSpec),
    Market(MarketFilterSpec),
    Pool(PoolFilterSpec),
    Protocol(ProtocolFilterSpec),
}

/// Extract a typed filter spec from an `IndexTarget`.
///
/// - For target kinds where `filter_spec` is optional, returns a default spec
///   when the field is `None`.
/// - For target kinds where `filter_spec` is required (`topic_filter`,
///   `protocol`), returns an error when the field is `None`.
pub fn extract_filter_spec(target: &IndexTarget) -> anyhow::Result<TypedFilterSpec> {
    match target.kind {
        TargetKind::Wallet => {
            let spec = match &target.filter_spec {
                Some(v) => serde_json::from_value(v.clone())?,
                None => WalletFilterSpec::default(),
            };
            Ok(TypedFilterSpec::Wallet(spec))
        }
        TargetKind::Contract => {
            let spec = match &target.filter_spec {
                Some(v) => serde_json::from_value(v.clone())?,
                None => ContractFilterSpec::default(),
            };
            Ok(TypedFilterSpec::Contract(spec))
        }
        TargetKind::Program => {
            let spec = match &target.filter_spec {
                Some(v) => serde_json::from_value(v.clone())?,
                None => ProgramFilterSpec::default(),
            };
            Ok(TypedFilterSpec::Program(spec))
        }
        TargetKind::Account => {
            let spec = match &target.filter_spec {
                Some(v) => serde_json::from_value(v.clone())?,
                None => AccountFilterSpec::default(),
            };
            Ok(TypedFilterSpec::Account(spec))
        }
        TargetKind::TopicFilter => {
            let v = target
                .filter_spec
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("topic_filter target requires filter_spec"))?;
            let spec: TopicFilterSpec = serde_json::from_value(v.clone())?;
            Ok(TypedFilterSpec::TopicFilter(spec))
        }
        TargetKind::Market => {
            let spec = match &target.filter_spec {
                Some(v) => serde_json::from_value(v.clone())?,
                None => MarketFilterSpec::default(),
            };
            Ok(TypedFilterSpec::Market(spec))
        }
        TargetKind::Pool => {
            let spec = match &target.filter_spec {
                Some(v) => serde_json::from_value(v.clone())?,
                None => PoolFilterSpec::default(),
            };
            Ok(TypedFilterSpec::Pool(spec))
        }
        TargetKind::Protocol => {
            let v = target
                .filter_spec
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("protocol target requires filter_spec"))?;
            let spec: ProtocolFilterSpec = serde_json::from_value(v.clone())?;
            Ok(TypedFilterSpec::Protocol(spec))
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that a target is well-formed and can be serviced.
///
/// Checks:
/// 1. The target kind is valid for its chain family.
/// 2. Address is present when required (all kinds except topic_filter, protocol).
/// 3. filter_spec is present when required (topic_filter, protocol).
pub fn validate_target(target: &IndexTarget) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();

    // Rule 1: kind must be valid for chain family
    if !target.kind.valid_for_chain_family(target.chain_family) {
        errors.push(format!(
            "target kind {:?} is not valid for chain family {:?}",
            target.kind, target.chain_family
        ));
    }

    // Rule 2: address required for simple target kinds
    let address_required = !matches!(target.kind, TargetKind::TopicFilter | TargetKind::Protocol);
    if address_required && target.address.as_ref().is_none_or(|a| a.is_empty()) {
        errors.push(format!(
            "target kind {:?} requires a non-empty address",
            target.kind
        ));
    }

    // Rule 3: filter_spec required for complex target kinds
    let filter_required = matches!(target.kind, TargetKind::TopicFilter | TargetKind::Protocol);
    if filter_required && target.filter_spec.is_none() {
        errors.push(format!(
            "target kind {:?} requires filter_spec",
            target.kind
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::IndexTarget;
    use chrono::Utc;
    use uuid::Uuid;

    fn make_target(
        kind: TargetKind,
        chain_family: ChainFamily,
        address: Option<&str>,
        filter_spec: Option<serde_json::Value>,
        mode: TargetMode,
    ) -> IndexTarget {
        let now = Utc::now();
        IndexTarget {
            id: Uuid::new_v4(),
            kind,
            network: "test-mainnet".to_string(),
            chain_family,
            address: address.map(|s| s.to_string()),
            filter_spec,
            mode,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    // -- ConnectorCapabilities --

    #[test]
    fn capabilities_supports_target_kind() {
        let caps = ConnectorCapabilities {
            supported_target_kinds: vec![TargetKind::Wallet, TargetKind::Contract],
            supported_modes: vec![TargetMode::Backfill],
            chain_family: ChainFamily::Evm,
        };
        assert!(caps.supports_target_kind(TargetKind::Wallet));
        assert!(caps.supports_target_kind(TargetKind::Contract));
        assert!(!caps.supports_target_kind(TargetKind::Program));
    }

    #[test]
    fn capabilities_supports_mode() {
        let caps = ConnectorCapabilities {
            supported_target_kinds: vec![TargetKind::Wallet],
            supported_modes: vec![TargetMode::Backfill],
            chain_family: ChainFamily::Evm,
        };
        assert!(caps.supports_mode(TargetMode::Backfill));
        assert!(!caps.supports_mode(TargetMode::Stream));
        // Both requires either Backfill or Both
        assert!(caps.supports_mode(TargetMode::Both));
    }

    #[test]
    fn capabilities_supports_mode_both() {
        let caps = ConnectorCapabilities {
            supported_target_kinds: vec![TargetKind::Wallet],
            supported_modes: vec![TargetMode::Both],
            chain_family: ChainFamily::Solana,
        };
        assert!(caps.supports_mode(TargetMode::Both));
        // Both in the connector means it supports the combined mode
    }

    #[test]
    fn capabilities_can_service() {
        let caps = ConnectorCapabilities {
            supported_target_kinds: vec![TargetKind::Wallet, TargetKind::Program],
            supported_modes: vec![TargetMode::Backfill],
            chain_family: ChainFamily::Solana,
        };

        let wallet_target = make_target(
            TargetKind::Wallet,
            ChainFamily::Solana,
            Some("abc"),
            None,
            TargetMode::Backfill,
        );
        assert!(caps.can_service(&wallet_target));

        let evm_wallet = make_target(
            TargetKind::Wallet,
            ChainFamily::Evm,
            Some("0xabc"),
            None,
            TargetMode::Backfill,
        );
        assert!(!caps.can_service(&evm_wallet)); // wrong chain family

        let contract = make_target(
            TargetKind::Contract,
            ChainFamily::Solana,
            Some("0xabc"),
            None,
            TargetMode::Backfill,
        );
        assert!(!caps.can_service(&contract)); // unsupported kind
    }

    // -- Typed filter spec extraction --

    #[test]
    fn extract_wallet_filter_default() {
        let target = make_target(
            TargetKind::Wallet,
            ChainFamily::Solana,
            Some("abc"),
            None,
            TargetMode::Both,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Wallet(w) => {
                assert!(w.asset_filter.is_empty());
                assert!(w.min_amount.is_none());
                assert!(w.directions.is_empty());
            }
            _ => panic!("expected WalletFilterSpec"),
        }
    }

    #[test]
    fn extract_wallet_filter_with_spec() {
        let filter = serde_json::json!({
            "asset_filter": ["SOL", "USDC"],
            "min_amount": "1.0",
            "directions": ["inbound"]
        });
        let target = make_target(
            TargetKind::Wallet,
            ChainFamily::Solana,
            Some("abc"),
            Some(filter),
            TargetMode::Both,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Wallet(w) => {
                assert_eq!(w.asset_filter, vec!["SOL", "USDC"]);
                assert_eq!(w.min_amount.as_deref(), Some("1.0"));
                assert_eq!(w.directions, vec!["inbound"]);
            }
            _ => panic!("expected WalletFilterSpec"),
        }
    }

    #[test]
    fn extract_contract_filter_default() {
        let target = make_target(
            TargetKind::Contract,
            ChainFamily::Evm,
            Some("0xusdc"),
            None,
            TargetMode::Backfill,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Contract(c) => {
                assert!(c.event_signatures.is_empty());
                assert!(c.topic_filters.is_none());
            }
            _ => panic!("expected ContractFilterSpec"),
        }
    }

    #[test]
    fn extract_contract_filter_with_events() {
        let filter = serde_json::json!({
            "event_signatures": ["Transfer(address,address,uint256)"]
        });
        let target = make_target(
            TargetKind::Contract,
            ChainFamily::Evm,
            Some("0xusdc"),
            Some(filter),
            TargetMode::Backfill,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Contract(c) => {
                assert_eq!(
                    c.event_signatures,
                    vec!["Transfer(address,address,uint256)"]
                );
            }
            _ => panic!("expected ContractFilterSpec"),
        }
    }

    #[test]
    fn extract_program_filter_default() {
        let target = make_target(
            TargetKind::Program,
            ChainFamily::Solana,
            Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            None,
            TargetMode::Stream,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Program(p) => {
                assert!(p.instruction_discriminators.is_empty());
                assert!(p.include_cpi); // default true
            }
            _ => panic!("expected ProgramFilterSpec"),
        }
    }

    #[test]
    fn extract_program_filter_with_discriminators() {
        let filter = serde_json::json!({
            "instruction_discriminators": ["0x03"],
            "include_cpi": false
        });
        let target = make_target(
            TargetKind::Program,
            ChainFamily::Solana,
            Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
            Some(filter),
            TargetMode::Stream,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Program(p) => {
                assert_eq!(p.instruction_discriminators, vec!["0x03"]);
                assert!(!p.include_cpi);
            }
            _ => panic!("expected ProgramFilterSpec"),
        }
    }

    #[test]
    fn extract_account_filter_default() {
        let target = make_target(
            TargetKind::Account,
            ChainFamily::Solana,
            Some("8szGkuLTAux9XMgZ2vtY39jVSowEcpBfFfD8hXSEqdGC"),
            None,
            TargetMode::Both,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Account(a) => {
                assert!(a.access_types.is_empty());
                assert!(a.associated_program.is_none());
            }
            _ => panic!("expected AccountFilterSpec"),
        }
    }

    #[test]
    fn extract_topic_filter_requires_spec() {
        let target = make_target(
            TargetKind::TopicFilter,
            ChainFamily::Evm,
            None,
            None,
            TargetMode::Backfill,
        );
        let result = extract_filter_spec(&target);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires filter_spec"));
    }

    #[test]
    fn extract_topic_filter_with_spec() {
        let filter = serde_json::json!({
            "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                null,
                "0x000000000000000000000000742d35cc6634c0532925a3b844bc9e7595f2bd18"
            ],
            "address_filter": ["0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"]
        });
        let target = make_target(
            TargetKind::TopicFilter,
            ChainFamily::Evm,
            None,
            Some(filter),
            TargetMode::Backfill,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::TopicFilter(t) => {
                assert_eq!(t.topics.len(), 3);
                assert_eq!(t.address_filter.len(), 1);
            }
            _ => panic!("expected TopicFilterSpec"),
        }
    }

    #[test]
    fn extract_market_filter_default() {
        let target = make_target(
            TargetKind::Market,
            ChainFamily::Hyperliquid,
            Some("ETH"),
            None,
            TargetMode::Both,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Market(m) => {
                assert!(m.data_types.is_empty());
                assert!(m.min_size.is_none());
            }
            _ => panic!("expected MarketFilterSpec"),
        }
    }

    #[test]
    fn extract_market_filter_with_data_types() {
        let filter = serde_json::json!({
            "data_types": ["fills", "funding"],
            "min_size": "1.0"
        });
        let target = make_target(
            TargetKind::Market,
            ChainFamily::Hyperliquid,
            Some("ETH"),
            Some(filter),
            TargetMode::Both,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Market(m) => {
                assert_eq!(m.data_types, vec!["fills", "funding"]);
                assert_eq!(m.min_size.as_deref(), Some("1.0"));
            }
            _ => panic!("expected MarketFilterSpec"),
        }
    }

    #[test]
    fn extract_pool_filter_default() {
        let target = make_target(
            TargetKind::Pool,
            ChainFamily::Evm,
            Some("0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640"),
            None,
            TargetMode::Backfill,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Pool(p) => {
                assert!(p.protocol.is_none());
                assert!(p.token_pair.is_empty());
                assert!(p.event_types.is_empty());
            }
            _ => panic!("expected PoolFilterSpec"),
        }
    }

    #[test]
    fn extract_pool_filter_with_metadata() {
        let filter = serde_json::json!({
            "protocol": "uniswap_v3",
            "token_pair": ["USDC", "WETH"],
            "event_types": ["Swap", "Mint"]
        });
        let target = make_target(
            TargetKind::Pool,
            ChainFamily::Evm,
            Some("0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640"),
            Some(filter),
            TargetMode::Backfill,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Pool(p) => {
                assert_eq!(p.protocol.as_deref(), Some("uniswap_v3"));
                assert_eq!(p.token_pair, vec!["USDC", "WETH"]);
                assert_eq!(p.event_types, vec!["Swap", "Mint"]);
            }
            _ => panic!("expected PoolFilterSpec"),
        }
    }

    #[test]
    fn extract_protocol_filter_requires_spec() {
        let target = make_target(
            TargetKind::Protocol,
            ChainFamily::Evm,
            None,
            None,
            TargetMode::Backfill,
        );
        let result = extract_filter_spec(&target);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires filter_spec"));
    }

    #[test]
    fn extract_protocol_filter_with_spec() {
        let filter = serde_json::json!({
            "name": "aave_v3",
            "addresses": {
                "ethereum-mainnet": [
                    {
                        "address": "0x87870bca3f3fd6335c3f4ce8392d69350b4fa4e2",
                        "role": "pool",
                        "label": "Aave V3 Pool"
                    }
                ]
            }
        });
        let target = make_target(
            TargetKind::Protocol,
            ChainFamily::Evm,
            None,
            Some(filter),
            TargetMode::Backfill,
        );
        let spec = extract_filter_spec(&target).unwrap();
        match spec {
            TypedFilterSpec::Protocol(p) => {
                assert_eq!(p.name, "aave_v3");
                assert_eq!(p.addresses.len(), 1);
                let eth_addrs = &p.addresses["ethereum-mainnet"];
                assert_eq!(eth_addrs.len(), 1);
                assert_eq!(eth_addrs[0].role, "pool");
            }
            _ => panic!("expected ProtocolFilterSpec"),
        }
    }

    // -- Validation --

    #[test]
    fn validate_target_valid_wallet() {
        let target = make_target(
            TargetKind::Wallet,
            ChainFamily::Solana,
            Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"),
            None,
            TargetMode::Both,
        );
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn validate_target_valid_contract() {
        let target = make_target(
            TargetKind::Contract,
            ChainFamily::Evm,
            Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            None,
            TargetMode::Backfill,
        );
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn validate_target_invalid_kind_for_family() {
        let target = make_target(
            TargetKind::Contract,
            ChainFamily::Solana,
            Some("abc"),
            None,
            TargetMode::Backfill,
        );
        let errors = validate_target(&target).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.contains("not valid for chain family")));
    }

    #[test]
    fn validate_target_missing_address() {
        let target = make_target(
            TargetKind::Wallet,
            ChainFamily::Solana,
            None,
            None,
            TargetMode::Both,
        );
        let errors = validate_target(&target).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.contains("requires a non-empty address")));
    }

    #[test]
    fn validate_target_empty_address() {
        let target = make_target(
            TargetKind::Wallet,
            ChainFamily::Solana,
            Some(""),
            None,
            TargetMode::Both,
        );
        let errors = validate_target(&target).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.contains("requires a non-empty address")));
    }

    #[test]
    fn validate_target_topic_filter_no_address_ok() {
        let filter = serde_json::json!({
            "topics": ["0xddf252ad"]
        });
        let target = make_target(
            TargetKind::TopicFilter,
            ChainFamily::Evm,
            None,
            Some(filter),
            TargetMode::Backfill,
        );
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn validate_target_topic_filter_missing_spec() {
        let target = make_target(
            TargetKind::TopicFilter,
            ChainFamily::Evm,
            None,
            None,
            TargetMode::Backfill,
        );
        let errors = validate_target(&target).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("requires filter_spec")));
    }

    #[test]
    fn validate_target_protocol_missing_spec() {
        let target = make_target(
            TargetKind::Protocol,
            ChainFamily::Evm,
            None,
            None,
            TargetMode::Backfill,
        );
        let errors = validate_target(&target).unwrap_err();
        assert!(errors.iter().any(|e| e.contains("requires filter_spec")));
    }

    #[test]
    fn validate_target_protocol_with_spec_ok() {
        let filter = serde_json::json!({
            "name": "aave",
            "addresses": {}
        });
        let target = make_target(
            TargetKind::Protocol,
            ChainFamily::Evm,
            None,
            Some(filter),
            TargetMode::Backfill,
        );
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn validate_target_multiple_errors() {
        // Contract on Solana with no address
        let target = make_target(
            TargetKind::Contract,
            ChainFamily::Solana,
            None,
            None,
            TargetMode::Backfill,
        );
        let errors = validate_target(&target).unwrap_err();
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn validate_target_market_hyperliquid() {
        let target = make_target(
            TargetKind::Market,
            ChainFamily::Hyperliquid,
            Some("ETH"),
            None,
            TargetMode::Both,
        );
        assert!(validate_target(&target).is_ok());
    }

    #[test]
    fn validate_target_market_wrong_family() {
        let target = make_target(
            TargetKind::Market,
            ChainFamily::Evm,
            Some("ETH"),
            None,
            TargetMode::Both,
        );
        let errors = validate_target(&target).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.contains("not valid for chain family")));
    }

    // -- Filter spec serde roundtrip --

    #[test]
    fn wallet_filter_spec_serde_roundtrip() {
        let spec = WalletFilterSpec {
            asset_filter: vec!["SOL".to_string(), "USDC".to_string()],
            min_amount: Some("1.0".to_string()),
            directions: vec!["inbound".to_string()],
        };
        let json = serde_json::to_value(&spec).unwrap();
        let back: WalletFilterSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.asset_filter, spec.asset_filter);
        assert_eq!(back.min_amount, spec.min_amount);
    }

    #[test]
    fn wallet_filter_spec_empty_fields_omitted() {
        let spec = WalletFilterSpec::default();
        let json = serde_json::to_value(&spec).unwrap();
        // Empty vecs and None should be omitted
        assert!(json.get("asset_filter").is_none());
        assert!(json.get("min_amount").is_none());
        assert!(json.get("directions").is_none());
    }

    #[test]
    fn contract_filter_spec_serde_roundtrip() {
        let spec = ContractFilterSpec {
            event_signatures: vec!["Transfer(address,address,uint256)".to_string()],
            topic_filters: None,
        };
        let json = serde_json::to_value(&spec).unwrap();
        let back: ContractFilterSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.event_signatures, spec.event_signatures);
    }

    #[test]
    fn topic_filter_spec_serde_roundtrip() {
        let spec = TopicFilterSpec {
            topics: vec![
                serde_json::json!("0xddf252ad"),
                serde_json::Value::Null,
                serde_json::json!(["0xaaa", "0xbbb"]),
            ],
            address_filter: vec!["0xusdc".to_string()],
        };
        let json = serde_json::to_value(&spec).unwrap();
        let back: TopicFilterSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.topics.len(), 3);
        assert_eq!(back.address_filter.len(), 1);
    }

    #[test]
    fn protocol_filter_spec_serde_roundtrip() {
        let spec = ProtocolFilterSpec {
            name: "aave_v3".to_string(),
            addresses: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "ethereum-mainnet".to_string(),
                    vec![ProtocolAddress {
                        address: "0x87870bca".to_string(),
                        role: "pool".to_string(),
                        label: Some("Pool".to_string()),
                    }],
                );
                m
            },
        };
        let json = serde_json::to_value(&spec).unwrap();
        let back: ProtocolFilterSpec = serde_json::from_value(json).unwrap();
        assert_eq!(back.name, "aave_v3");
        assert_eq!(back.addresses["ethereum-mainnet"][0].role, "pool");
    }

    #[test]
    fn program_filter_default_include_cpi_is_true() {
        let spec: ProgramFilterSpec = serde_json::from_str("{}").unwrap();
        assert!(spec.include_cpi);
    }
}
