//! Provider registry for multi-network, multi-provider resolution.
//!
//! This module introduces:
//!
//! - `NetworkId` — typed newtype for network identifiers (e.g. "solana-mainnet")
//! - `ProviderKind` — the type of endpoint (rpc, grpc, ws, trace)
//! - `ProviderCapability` — what a provider can do (historical, balances, stream, etc.)
//! - `ResolvedProvider` — a fully-resolved provider entry ready for use
//! - `ProviderRegistry` — resolves the best provider for a (network, capability) pair

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};

use crate::config::{NetworkConfig, ProviderConfig};

// ---------------------------------------------------------------------------
// NetworkId
// ---------------------------------------------------------------------------

/// Typed newtype for network identifiers.
///
/// Uses kebab-case slug format: `solana-mainnet`, `ethereum-mainnet`,
/// `base-mainnet`, `arbitrum-mainnet`, `hypercore-mainnet`, etc.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NetworkId(String);

impl NetworkId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NetworkId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for NetworkId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for NetworkId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl AsRef<str> for NetworkId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// ProviderKind
// ---------------------------------------------------------------------------

/// The type of provider endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ProviderKind {
    /// Standard JSON-RPC endpoint (Solana RPC, EVM `eth_*`).
    Rpc,
    /// gRPC endpoint (e.g. Yellowstone for Solana).
    Grpc,
    /// WebSocket endpoint (e.g. Hyperliquid WS feed).
    Ws,
    /// Trace-capable RPC endpoint (EVM `debug_traceTransaction`).
    Trace,
    /// REST API endpoint (e.g. Hyperliquid info API).
    Rest,
}

// ---------------------------------------------------------------------------
// ProviderCapability
// ---------------------------------------------------------------------------

/// What a provider endpoint can do.
///
/// Connectors request a (network, capability) pair and the registry resolves
/// the best available provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProviderCapability {
    /// Fetch historical transactions by signature/hash.
    Historical,
    /// Query token/native balances.
    Balances,
    /// Real-time streaming (gRPC, WebSocket, etc.).
    Stream,
    /// EVM event log queries (`eth_getLogs`).
    Logs,
    /// EVM transaction receipt retrieval.
    Receipts,
    /// EVM `debug_traceTransaction` support.
    DebugTraceTransaction,
}

// ---------------------------------------------------------------------------
// ResolvedProvider
// ---------------------------------------------------------------------------

/// A fully-resolved provider entry ready for connector use.
#[derive(Clone)]
pub struct ResolvedProvider {
    /// Network this provider serves.
    pub network: NetworkId,
    /// Kind of endpoint.
    pub kind: ProviderKind,
    /// URL to connect to.
    pub url: String,
    /// Priority (lower number = higher priority).
    pub priority: u32,
    /// Capabilities this provider offers.
    pub capabilities: Vec<ProviderCapability>,
    /// Optional auth token (resolved from env var if `token_env` was set).
    pub token: Option<String>,
    /// Optional extra headers.
    pub headers: HashMap<String, String>,
}

impl std::fmt::Debug for ResolvedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProvider")
            .field("network", &self.network)
            .field("kind", &self.kind)
            .field("url", &self.url)
            .field("priority", &self.priority)
            .field("capabilities", &self.capabilities)
            .field("token", &"[REDACTED]")
            .field("headers", &self.headers)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// ProviderRegistry
// ---------------------------------------------------------------------------

/// Resolves the best provider for a (network, capability) pair.
///
/// Built from `AppConfig.networks` and `AppConfig.providers` during startup.
/// The registry is immutable once constructed.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    /// All registered providers, grouped by network.
    providers: HashMap<NetworkId, Vec<ResolvedProvider>>,
    /// Enabled networks.
    enabled_networks: HashMap<NetworkId, NetworkConfig>,
}

impl ProviderRegistry {
    /// Build a registry from config-level network and provider definitions.
    ///
    /// - Filters out providers for disabled networks.
    /// - Resolves `token_env` references to actual values via `std::env::var`.
    /// - Sorts providers by priority within each network.
    pub fn from_config(
        networks: &HashMap<String, NetworkConfig>,
        providers: &[ProviderConfig],
    ) -> Result<Self, RegistryError> {
        let enabled: HashMap<NetworkId, NetworkConfig> = networks
            .iter()
            .filter(|(_, nc)| nc.enabled)
            .map(|(k, v)| (NetworkId::new(k.clone()), v.clone()))
            .collect();

        let mut grouped: HashMap<NetworkId, Vec<ResolvedProvider>> = HashMap::new();

        for pc in providers {
            let net_id = NetworkId::new(&pc.network);

            // Skip providers for networks that are not enabled.
            if !enabled.contains_key(&net_id) {
                continue;
            }

            if pc.url.trim().is_empty() {
                return Err(RegistryError::Validation(format!(
                    "provider for network '{}' has an empty URL",
                    pc.network
                )));
            }

            // Resolve the auth token: direct `token` value takes precedence,
            // then `token_env` is looked up from the environment, then None.
            let token = pc.token.clone().or_else(|| {
                pc.token_env
                    .as_ref()
                    .and_then(|env_var| std::env::var(env_var).ok())
            });

            // Parse capabilities.
            let capabilities: Vec<ProviderCapability> = pc
                .capabilities
                .iter()
                .map(|s| {
                    s.parse::<ProviderCapability>().map_err(|_| {
                        RegistryError::Validation(format!(
                            "unknown provider capability '{}' for network '{}'",
                            s, pc.network
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            if capabilities.is_empty() {
                return Err(RegistryError::Validation(format!(
                    "provider for network '{}' (kind={}) has no capabilities",
                    pc.network, pc.kind
                )));
            }

            let kind: ProviderKind = pc.kind.parse::<ProviderKind>().map_err(|_| {
                RegistryError::Validation(format!(
                    "unknown provider kind '{}' for network '{}'",
                    pc.kind, pc.network
                ))
            })?;

            let resolved = ResolvedProvider {
                network: net_id.clone(),
                kind,
                url: pc.url.clone(),
                priority: pc.priority.unwrap_or(1),
                capabilities,
                token,
                headers: pc.headers.clone().unwrap_or_default(),
            };

            grouped.entry(net_id).or_default().push(resolved);
        }

        // Sort each network's providers by priority (ascending = highest priority first).
        for providers in grouped.values_mut() {
            providers.sort_by_key(|p| p.priority);
        }

        Ok(Self {
            providers: grouped,
            enabled_networks: enabled,
        })
    }

    /// Resolve the best provider for the given network and capability.
    ///
    /// Returns the highest-priority (lowest number) provider that declares
    /// the requested capability, or `None` if no match is found.
    pub fn resolve(
        &self,
        network: &NetworkId,
        capability: ProviderCapability,
    ) -> Option<&ResolvedProvider> {
        self.providers.get(network).and_then(|providers| {
            providers
                .iter()
                .find(|p| p.capabilities.contains(&capability))
        })
    }

    /// Resolve all providers for the given network and capability, ordered
    /// by priority (best first). Useful for fallback chains.
    pub fn resolve_all(
        &self,
        network: &NetworkId,
        capability: ProviderCapability,
    ) -> Vec<&ResolvedProvider> {
        match self.providers.get(network) {
            Some(providers) => providers
                .iter()
                .filter(|p| p.capabilities.contains(&capability))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Returns all providers registered for a given network.
    pub fn providers_for_network(&self, network: &NetworkId) -> &[ResolvedProvider] {
        self.providers
            .get(network)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Returns an iterator over all enabled network IDs.
    pub fn enabled_networks(&self) -> impl Iterator<Item = &NetworkId> {
        self.enabled_networks.keys()
    }

    /// Returns `true` if the given network is enabled.
    pub fn is_network_enabled(&self, network: &NetworkId) -> bool {
        self.enabled_networks.contains_key(network)
    }

    /// Returns the `NetworkConfig` for a given network ID, if enabled.
    pub fn network_config(&self, network: &NetworkId) -> Option<&NetworkConfig> {
        self.enabled_networks.get(network)
    }

    /// Returns `true` if any provider for the given network declares the
    /// given capability.
    pub fn has_capability(&self, network: &NetworkId, capability: ProviderCapability) -> bool {
        self.resolve(network, capability).is_some()
    }
}

// ---------------------------------------------------------------------------
// NetworkContext
// ---------------------------------------------------------------------------

/// Bundles what a connector needs from the provider registry.
///
/// A `NetworkContext` is the connector-facing view of a resolved network
/// configuration. It carries the network identity, resolved provider URLs
/// by capability, and any auth tokens. Connectors accept a `NetworkContext`
/// instead of raw URL strings, decoupling adapter construction from
/// environment-specific configuration details.
#[derive(Debug, Clone)]
pub struct NetworkContext {
    /// The network this context represents.
    pub network: NetworkId,
    /// Resolved providers available for this network, keyed by capability.
    /// When multiple providers support the same capability, only the
    /// highest-priority one is included here (use the registry directly
    /// for fallback chains).
    providers: HashMap<ProviderCapability, ResolvedProvider>,
}

impl NetworkContext {
    /// Build a `NetworkContext` from the registry for the given network.
    ///
    /// Resolves the best provider for each known capability and bundles
    /// them into the context. Returns `None` if the network is not enabled
    /// or has no providers.
    pub fn from_registry(registry: &ProviderRegistry, network: &NetworkId) -> Option<Self> {
        if !registry.is_network_enabled(network) {
            return None;
        }

        let all_capabilities = [
            ProviderCapability::Historical,
            ProviderCapability::Balances,
            ProviderCapability::Stream,
            ProviderCapability::Logs,
            ProviderCapability::Receipts,
            ProviderCapability::DebugTraceTransaction,
        ];

        let mut providers = HashMap::new();
        for cap in &all_capabilities {
            if let Some(p) = registry.resolve(network, *cap) {
                providers.insert(*cap, p.clone());
            }
        }

        Some(Self {
            network: network.clone(),
            providers,
        })
    }

    /// Get the resolved provider for a specific capability.
    pub fn provider(&self, capability: ProviderCapability) -> Option<&ResolvedProvider> {
        self.providers.get(&capability)
    }

    /// Get the URL for a specific capability, if available.
    pub fn url(&self, capability: ProviderCapability) -> Option<&str> {
        self.providers.get(&capability).map(|p| p.url.as_str())
    }

    /// Get the auth token for a specific capability, if available.
    pub fn token(&self, capability: ProviderCapability) -> Option<&str> {
        self.providers
            .get(&capability)
            .and_then(|p| p.token.as_deref())
    }

    /// Returns `true` if this context has a provider for the given capability.
    pub fn has_capability(&self, capability: ProviderCapability) -> bool {
        self.providers.contains_key(&capability)
    }

    /// Returns all capabilities available in this context.
    pub fn available_capabilities(&self) -> Vec<ProviderCapability> {
        self.providers.keys().copied().collect()
    }
}

/// Map a legacy V1 `chain` string to its default network ID.
///
/// This provides backward compatibility: API and CLI callers that still
/// pass `chain = "solana"` get mapped to `solana-mainnet`, etc.
pub fn chain_to_network_id(chain: &str) -> Option<NetworkId> {
    match chain {
        "solana" => Some(NetworkId::new("solana-mainnet")),
        "ethereum" => Some(NetworkId::new("ethereum-mainnet")),
        "hyperliquid" => Some(NetworkId::new("hypercore-mainnet")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// RegistryError
// ---------------------------------------------------------------------------

/// Errors from provider registry construction.
#[derive(Debug)]
pub enum RegistryError {
    /// A validated field has an invalid value.
    Validation(String),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Validation(msg) => write!(f, "provider registry error: {msg}"),
        }
    }
}

impl std::error::Error for RegistryError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NetworkConfig, ProviderConfig};

    fn make_networks() -> HashMap<String, NetworkConfig> {
        let mut m = HashMap::new();
        m.insert(
            "solana-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );
        m.insert(
            "ethereum-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );
        m.insert("base-mainnet".to_string(), NetworkConfig { enabled: false });
        m
    }

    fn make_providers() -> Vec<ProviderConfig> {
        vec![
            ProviderConfig {
                network: "solana-mainnet".to_string(),
                kind: "rpc".to_string(),
                url: "https://api.mainnet-beta.solana.com".to_string(),
                priority: Some(1),
                capabilities: vec!["historical".to_string(), "balances".to_string()],
                token_env: None,
                token: None,
                headers: None,
            },
            ProviderConfig {
                network: "solana-mainnet".to_string(),
                kind: "grpc".to_string(),
                url: "https://yellowstone.example.com".to_string(),
                priority: Some(1),
                capabilities: vec!["stream".to_string()],
                token_env: Some("SOLANA_GRPC_TOKEN".to_string()),
                token: None,
                headers: None,
            },
            ProviderConfig {
                network: "ethereum-mainnet".to_string(),
                kind: "rpc".to_string(),
                url: "https://eth.llamarpc.com".to_string(),
                priority: Some(1),
                capabilities: vec![
                    "historical".to_string(),
                    "logs".to_string(),
                    "receipts".to_string(),
                    "balances".to_string(),
                ],
                token_env: None,
                token: None,
                headers: None,
            },
            ProviderConfig {
                network: "ethereum-mainnet".to_string(),
                kind: "trace".to_string(),
                url: "https://eth-trace.alchemy.com/v2/KEY".to_string(),
                priority: Some(2),
                capabilities: vec!["debug_trace_transaction".to_string()],
                token_env: None,
                token: None,
                headers: None,
            },
            // Provider for disabled network — should be filtered out.
            ProviderConfig {
                network: "base-mainnet".to_string(),
                kind: "rpc".to_string(),
                url: "https://base.llamarpc.com".to_string(),
                priority: Some(1),
                capabilities: vec!["historical".to_string()],
                token_env: None,
                token: None,
                headers: None,
            },
        ]
    }

    #[test]
    fn registry_filters_disabled_networks() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let base = NetworkId::new("base-mainnet");
        assert!(!registry.is_network_enabled(&base));
        assert!(registry.providers_for_network(&base).is_empty());
    }

    #[test]
    fn registry_resolves_best_provider() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let sol = NetworkId::new("solana-mainnet");

        let historical = registry
            .resolve(&sol, ProviderCapability::Historical)
            .unwrap();
        assert_eq!(historical.kind, ProviderKind::Rpc);
        assert_eq!(historical.url, "https://api.mainnet-beta.solana.com");

        let stream = registry.resolve(&sol, ProviderCapability::Stream).unwrap();
        assert_eq!(stream.kind, ProviderKind::Grpc);
    }

    #[test]
    fn registry_resolves_none_for_missing_capability() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let sol = NetworkId::new("solana-mainnet");
        assert!(registry
            .resolve(&sol, ProviderCapability::DebugTraceTransaction)
            .is_none());
    }

    #[test]
    fn registry_resolves_none_for_unknown_network() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let unknown = NetworkId::new("polygon-mainnet");
        assert!(registry
            .resolve(&unknown, ProviderCapability::Historical)
            .is_none());
    }

    #[test]
    fn registry_resolve_all_returns_priority_order() {
        let mut networks = HashMap::new();
        networks.insert("eth-mainnet".to_string(), NetworkConfig { enabled: true });

        let providers = vec![
            ProviderConfig {
                network: "eth-mainnet".to_string(),
                kind: "rpc".to_string(),
                url: "https://backup.rpc".to_string(),
                priority: Some(10),
                capabilities: vec!["historical".to_string()],
                token_env: None,
                token: None,
                headers: None,
            },
            ProviderConfig {
                network: "eth-mainnet".to_string(),
                kind: "rpc".to_string(),
                url: "https://primary.rpc".to_string(),
                priority: Some(1),
                capabilities: vec!["historical".to_string()],
                token_env: None,
                token: None,
                headers: None,
            },
        ];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("eth-mainnet");
        let all = registry.resolve_all(&net, ProviderCapability::Historical);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].url, "https://primary.rpc");
        assert_eq!(all[1].url, "https://backup.rpc");
    }

    #[test]
    fn registry_enabled_networks_iterator() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let enabled: Vec<&NetworkId> = registry.enabled_networks().collect();
        assert_eq!(enabled.len(), 2);
        // Should contain solana-mainnet and ethereum-mainnet but not base-mainnet.
        let ids: Vec<&str> = enabled.iter().map(|n| n.as_str()).collect();
        assert!(ids.contains(&"solana-mainnet"));
        assert!(ids.contains(&"ethereum-mainnet"));
        assert!(!ids.contains(&"base-mainnet"));
    }

    #[test]
    fn registry_has_capability() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let eth = NetworkId::new("ethereum-mainnet");
        assert!(registry.has_capability(&eth, ProviderCapability::Logs));
        assert!(registry.has_capability(&eth, ProviderCapability::DebugTraceTransaction));
        assert!(!registry.has_capability(&eth, ProviderCapability::Stream));
    }

    #[test]
    fn registry_rejects_empty_url() {
        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "rpc".to_string(),
            url: "".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let result = ProviderRegistry::from_config(&networks, &providers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("empty URL"));
    }

    #[test]
    fn registry_rejects_empty_capabilities() {
        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "rpc".to_string(),
            url: "https://rpc.test".to_string(),
            priority: Some(1),
            capabilities: vec![],
            token_env: None,
            token: None,
            headers: None,
        }];

        let result = ProviderRegistry::from_config(&networks, &providers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no capabilities"));
    }

    #[test]
    fn registry_rejects_unknown_capability() {
        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "rpc".to_string(),
            url: "https://rpc.test".to_string(),
            priority: Some(1),
            capabilities: vec!["time_travel".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let result = ProviderRegistry::from_config(&networks, &providers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown provider capability"));
    }

    #[test]
    fn registry_rejects_unknown_kind() {
        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "quantum".to_string(),
            url: "https://rpc.test".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let result = ProviderRegistry::from_config(&networks, &providers);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("unknown provider kind"));
    }

    #[test]
    fn registry_default_priority_is_one() {
        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "rpc".to_string(),
            url: "https://rpc.test".to_string(),
            priority: None,
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("test");
        let p = registry
            .resolve(&net, ProviderCapability::Historical)
            .unwrap();
        assert_eq!(p.priority, 1);
    }

    #[test]
    fn registry_token_env_resolution() {
        // SAFETY: env var mutation is unsafe since Rust 1.83, acceptable in sequential tests.
        unsafe {
            std::env::set_var("TEST_GRPC_TOKEN_XYZ", "my-secret-token");
        }

        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "grpc".to_string(),
            url: "https://grpc.test".to_string(),
            priority: Some(1),
            capabilities: vec!["stream".to_string()],
            token_env: Some("TEST_GRPC_TOKEN_XYZ".to_string()),
            token: None,
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("test");
        let p = registry.resolve(&net, ProviderCapability::Stream).unwrap();
        assert_eq!(p.token.as_deref(), Some("my-secret-token"));

        unsafe {
            std::env::remove_var("TEST_GRPC_TOKEN_XYZ");
        }
    }

    #[test]
    fn registry_direct_token_takes_precedence_over_env() {
        // Set an env var, but also provide a direct token value.
        // The direct value should win.
        // SAFETY: env var mutation is unsafe since Rust 1.83, acceptable in sequential tests.
        unsafe {
            std::env::set_var("TEST_GRPC_TOKEN_PRECEDENCE", "env-value");
        }

        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "grpc".to_string(),
            url: "https://grpc.test".to_string(),
            priority: Some(1),
            capabilities: vec!["stream".to_string()],
            token_env: Some("TEST_GRPC_TOKEN_PRECEDENCE".to_string()),
            token: Some("direct-value".to_string()),
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("test");
        let p = registry.resolve(&net, ProviderCapability::Stream).unwrap();
        assert_eq!(p.token.as_deref(), Some("direct-value"));

        unsafe {
            std::env::remove_var("TEST_GRPC_TOKEN_PRECEDENCE");
        }
    }

    #[test]
    fn registry_token_env_missing_is_none() {
        // Ensure the var does not exist.
        // SAFETY: env var mutation is unsafe since Rust 1.83, acceptable in sequential tests.
        unsafe {
            std::env::remove_var("DEFINITELY_NOT_SET_TOKEN_ABC123");
        }

        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "grpc".to_string(),
            url: "https://grpc.test".to_string(),
            priority: Some(1),
            capabilities: vec!["stream".to_string()],
            token_env: Some("DEFINITELY_NOT_SET_TOKEN_ABC123".to_string()),
            token: None,
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("test");
        let p = registry.resolve(&net, ProviderCapability::Stream).unwrap();
        assert!(p.token.is_none());
    }

    #[test]
    fn network_id_display_and_equality() {
        let a = NetworkId::new("solana-mainnet");
        let b = NetworkId::from("solana-mainnet".to_string());
        let c: NetworkId = "solana-mainnet".into();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(format!("{a}"), "solana-mainnet");
        assert_eq!(a.as_str(), "solana-mainnet");
    }

    #[test]
    fn provider_capability_parse() {
        assert_eq!(
            "historical".parse::<ProviderCapability>().unwrap(),
            ProviderCapability::Historical
        );
        assert_eq!(
            "debug_trace_transaction"
                .parse::<ProviderCapability>()
                .unwrap(),
            ProviderCapability::DebugTraceTransaction
        );
        assert!("unknown".parse::<ProviderCapability>().is_err());
    }

    #[test]
    fn provider_kind_parse() {
        assert_eq!("rpc".parse::<ProviderKind>().unwrap(), ProviderKind::Rpc);
        assert_eq!("grpc".parse::<ProviderKind>().unwrap(), ProviderKind::Grpc);
        assert_eq!(
            "trace".parse::<ProviderKind>().unwrap(),
            ProviderKind::Trace
        );
        assert_eq!("ws".parse::<ProviderKind>().unwrap(), ProviderKind::Ws);
        assert_eq!("rest".parse::<ProviderKind>().unwrap(), ProviderKind::Rest);
    }

    #[test]
    fn empty_registry_is_valid() {
        let networks: HashMap<String, NetworkConfig> = HashMap::new();
        let providers: Vec<ProviderConfig> = vec![];
        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        assert_eq!(registry.enabled_networks().count(), 0);
    }

    #[test]
    fn network_with_no_providers_is_valid() {
        let mut networks = HashMap::new();
        networks.insert("orphan".to_string(), NetworkConfig { enabled: true });

        let providers: Vec<ProviderConfig> = vec![];
        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        assert!(registry.is_network_enabled(&NetworkId::new("orphan")));
        assert!(registry
            .providers_for_network(&NetworkId::new("orphan"))
            .is_empty());
    }

    #[test]
    fn headers_are_preserved() {
        let mut networks = HashMap::new();
        networks.insert("test".to_string(), NetworkConfig { enabled: true });

        let mut hdrs = HashMap::new();
        hdrs.insert("Authorization".to_string(), "Bearer token123".to_string());

        let providers = vec![ProviderConfig {
            network: "test".to_string(),
            kind: "rpc".to_string(),
            url: "https://rpc.test".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: Some(hdrs),
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("test");
        let p = registry
            .resolve(&net, ProviderCapability::Historical)
            .unwrap();
        assert_eq!(
            p.headers.get("Authorization").map(|s| s.as_str()),
            Some("Bearer token123")
        );
    }

    // -- NetworkContext --

    #[test]
    fn network_context_from_registry() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let sol = NetworkId::new("solana-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &sol).unwrap();

        assert_eq!(ctx.network, sol);
        assert!(ctx.has_capability(ProviderCapability::Historical));
        assert!(ctx.has_capability(ProviderCapability::Balances));
        assert!(ctx.has_capability(ProviderCapability::Stream));
        assert!(!ctx.has_capability(ProviderCapability::Logs));
    }

    #[test]
    fn network_context_url_and_token() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let sol = NetworkId::new("solana-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &sol).unwrap();

        assert_eq!(
            ctx.url(ProviderCapability::Historical),
            Some("https://api.mainnet-beta.solana.com")
        );
        assert_eq!(
            ctx.url(ProviderCapability::Stream),
            Some("https://yellowstone.example.com")
        );
        assert!(ctx.url(ProviderCapability::Logs).is_none());
    }

    #[test]
    fn network_context_disabled_network_returns_none() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let base = NetworkId::new("base-mainnet");
        assert!(NetworkContext::from_registry(&registry, &base).is_none());
    }

    #[test]
    fn network_context_unknown_network_returns_none() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let unknown = NetworkId::new("polygon-mainnet");
        assert!(NetworkContext::from_registry(&registry, &unknown).is_none());
    }

    #[test]
    fn network_context_evm_has_all_evm_capabilities() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let eth = NetworkId::new("ethereum-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &eth).unwrap();

        assert!(ctx.has_capability(ProviderCapability::Historical));
        assert!(ctx.has_capability(ProviderCapability::Logs));
        assert!(ctx.has_capability(ProviderCapability::Receipts));
        assert!(ctx.has_capability(ProviderCapability::Balances));
        assert!(ctx.has_capability(ProviderCapability::DebugTraceTransaction));
        assert!(!ctx.has_capability(ProviderCapability::Stream));
    }

    #[test]
    fn network_context_available_capabilities() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let sol = NetworkId::new("solana-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &sol).unwrap();

        let caps = ctx.available_capabilities();
        assert!(caps.contains(&ProviderCapability::Historical));
        assert!(caps.contains(&ProviderCapability::Balances));
        assert!(caps.contains(&ProviderCapability::Stream));
        assert_eq!(caps.len(), 3);
    }

    #[test]
    fn network_context_provider_returns_full_resolved_provider() {
        let registry = ProviderRegistry::from_config(&make_networks(), &make_providers()).unwrap();
        let eth = NetworkId::new("ethereum-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &eth).unwrap();

        let p = ctx
            .provider(ProviderCapability::DebugTraceTransaction)
            .unwrap();
        assert_eq!(p.kind, ProviderKind::Trace);
        assert_eq!(p.url, "https://eth-trace.alchemy.com/v2/KEY");
    }

    // -- chain_to_network_id --

    #[test]
    fn chain_to_network_id_known_chains() {
        assert_eq!(
            chain_to_network_id("solana").unwrap().as_str(),
            "solana-mainnet"
        );
        assert_eq!(
            chain_to_network_id("ethereum").unwrap().as_str(),
            "ethereum-mainnet"
        );
        assert_eq!(
            chain_to_network_id("hyperliquid").unwrap().as_str(),
            "hypercore-mainnet"
        );
    }

    #[test]
    fn chain_to_network_id_unknown_returns_none() {
        assert!(chain_to_network_id("bitcoin").is_none());
        assert!(chain_to_network_id("").is_none());
    }
}
