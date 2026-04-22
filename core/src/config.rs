use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::provider::ProviderRegistry;

// ---------------------------------------------------------------------------
// Network and Provider config structs
// ---------------------------------------------------------------------------

/// Per-network enablement configuration.
///
/// Used inside the `[networks.<id>]` TOML table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Whether this network is enabled for ingestion and queries.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// A single provider endpoint definition.
///
/// Used inside the `[[providers]]` TOML array.
#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Network this provider serves (e.g. "solana-mainnet").
    pub network: String,
    /// Provider kind: "rpc", "grpc", "ws", "trace", "rest".
    pub kind: String,
    /// Endpoint URL.
    pub url: String,
    /// Priority (lower = preferred). Defaults to 1.
    #[serde(default)]
    pub priority: Option<u32>,
    /// Capabilities this provider offers (e.g. ["historical", "balances"]).
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Environment variable name containing an auth token.
    /// Resolved at registry construction time.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Direct auth token value. Takes precedence over `token_env`.
    /// Used during legacy config synthesis to pass through tokens that
    /// were set in the TOML config rather than via environment variables.
    #[serde(default)]
    pub token: Option<String>,
    /// Optional extra HTTP headers for this provider.
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("network", &self.network)
            .field("kind", &self.kind)
            .field("url", &self.url)
            .field("priority", &self.priority)
            .field("capabilities", &self.capabilities)
            .field("token_env", &"[REDACTED]")
            .field("token", &"[REDACTED]")
            .field("headers", &self.headers)
            .finish()
    }
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub port: u16,
    pub host: String,
    pub database_url: String,
    pub pool_size: u32,
    pub log_level: String,
    pub ingest_limit: usize,

    // -- Legacy singleton fields (deprecated, use `networks` + `providers`) --
    /// Solana RPC URL. Deprecated: configure via `[[providers]]` instead.
    #[serde(default = "default_solana_rpc_url")]
    pub solana_rpc_url: String,
    /// EVM RPC URL. Deprecated: configure via `[[providers]]` instead.
    #[serde(default = "default_evm_rpc_url")]
    pub evm_rpc_url: String,
    pub api_key: Option<String>,
    pub allowed_wallets: Option<String>,
    /// Solana gRPC URL. Deprecated: configure via `[[providers]]` instead.
    pub solana_grpc_url: Option<String>,
    /// Solana gRPC auth token. Deprecated: use `token_env` in provider config.
    pub solana_grpc_token: Option<String>,
    pub export_dir: String,
    /// Enable EVM trace fetching via `debug_traceTransaction`.
    /// Requires an RPC provider that supports the `debug` namespace (e.g.
    /// Alchemy, Infura with add-on, or a full/archive node).
    /// Disabled by default because most public RPC endpoints do not support it.
    pub evm_trace_enabled: bool,
    /// URL for the EVM debug/trace RPC endpoint. When set, trace requests
    /// use this URL instead of `evm_rpc_url`. This allows separating the
    /// standard RPC endpoint (which may be a free/public node) from the
    /// trace-capable endpoint (which often requires a paid tier).
    /// Deprecated: configure via `[[providers]]` with kind = "trace" instead.
    pub evm_trace_rpc_url: Option<String>,

    // -- V2 structured provider config --
    /// Per-network enablement. Keys are network IDs like "solana-mainnet".
    #[serde(default)]
    pub networks: HashMap<String, NetworkConfig>,

    /// Provider endpoint definitions.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,

    // -- Callback signing --
    /// Optional HMAC-SHA256 secret for signing callback payloads.
    ///
    /// Set via `SPECTRAPLEX_CALLBACK_HMAC_SECRET` environment variable or
    /// `callback_hmac_secret` in TOML.
    #[serde(default)]
    pub callback_hmac_secret: Option<String>,
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppConfig")
            .field("port", &self.port)
            .field("host", &self.host)
            .field("database_url", &"[REDACTED]")
            .field("pool_size", &self.pool_size)
            .field("log_level", &self.log_level)
            .field("api_key", &"[REDACTED]")
            .field("solana_grpc_token", &"[REDACTED]")
            .field("export_dir", &self.export_dir)
            .field("evm_trace_enabled", &self.evm_trace_enabled)
            .finish_non_exhaustive()
    }
}

fn default_solana_rpc_url() -> String {
    "https://api.mainnet-beta.solana.com".to_string()
}

fn default_evm_rpc_url() -> String {
    "https://eth.llamarpc.com".to_string()
}

/// Errors from config validation.
#[derive(Debug)]
pub enum ConfigError {
    /// Figment extraction failed.
    Figment(Box<figment::Error>),
    /// A validated field has an invalid value.
    Validation(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Figment(e) => write!(f, "config load error: {e}"),
            ConfigError::Validation(msg) => write!(f, "config validation error: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<Box<figment::Error>> for ConfigError {
    fn from(e: Box<figment::Error>) -> Self {
        ConfigError::Figment(e)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: 3000,
            host: "127.0.0.1".to_string(),
            database_url: String::new(),
            pool_size: 10,
            log_level: "info".to_string(),
            ingest_limit: 50,
            solana_rpc_url: default_solana_rpc_url(),
            evm_rpc_url: default_evm_rpc_url(),
            api_key: None,
            allowed_wallets: None,
            solana_grpc_url: None,
            solana_grpc_token: None,
            export_dir: "./exports".to_string(),
            evm_trace_enabled: false,
            evm_trace_rpc_url: None,
            networks: HashMap::new(),
            providers: Vec::new(),
            callback_hmac_secret: None,
        }
    }
}

impl AppConfig {
    /// Validate config fields that have semantic constraints.
    ///
    /// - `database_url` must not be empty (required for all database operations)
    /// - `port` must be non-zero (0 means OS-assigned, unusual and likely misconfiguration)
    /// - `pool_size` must be between 1 and 1000
    /// - `export_dir` must not be empty
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.database_url.trim().is_empty() {
            return Err(ConfigError::Validation(
                "database_url must not be empty".to_string(),
            ));
        }
        if self.port == 0 {
            return Err(ConfigError::Validation(
                "port must be between 1 and 65535".to_string(),
            ));
        }
        if self.pool_size == 0 || self.pool_size > 1000 {
            return Err(ConfigError::Validation(
                "pool_size must be between 1 and 1000".to_string(),
            ));
        }
        if self.export_dir.trim().is_empty() {
            return Err(ConfigError::Validation(
                "export_dir must not be empty".to_string(),
            ));
        }
        if let Some(ref secret) = self.callback_hmac_secret {
            if secret.trim().is_empty() {
                return Err(ConfigError::Validation(
                    "callback_hmac_secret must not be empty or whitespace-only".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn load() -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Serialized::defaults(AppConfig::default()))
            .merge(Toml::file("spectraplex.toml"))
            .merge(Env::prefixed("SPECTRAPLEX_"))
            .merge(Env::raw().only(&[
                "DATABASE_URL",
                "SOLANA_RPC_URL",
                "EVM_RPC_URL",
                "SOLANA_GRPC_URL",
                "SOLANA_GRPC_TOKEN",
                "EXPORT_DIR",
                "EVM_TRACE_RPC_URL",
            ]))
            .extract()
            .map_err(Box::new)
    }

    pub fn allowed_wallets_set(&self) -> Option<HashSet<String>> {
        self.allowed_wallets.as_ref().map(|s| {
            s.split(',')
                .map(|w| {
                    let trimmed = w.trim();
                    if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
                        trimmed.to_lowercase()
                    } else {
                        trimmed.to_string()
                    }
                })
                .filter(|w| !w.is_empty())
                .collect()
        })
    }

    /// Synthesize provider entries from legacy singleton fields.
    ///
    /// When `networks` and `providers` are empty (legacy config), this method
    /// creates equivalent structured entries from `solana_rpc_url`,
    /// `evm_rpc_url`, `solana_grpc_url`, and `evm_trace_rpc_url`.
    ///
    /// When the user supplies partial structured config (only `networks` or
    /// only `providers`), the synthesized defaults are merged with the user's
    /// entries so that staged migration works correctly.
    ///
    /// Returns the effective (networks, providers) tuple. If the user already
    /// supplied both structured sections, those are returned unchanged.
    pub fn effective_provider_config(
        &self,
    ) -> (HashMap<String, NetworkConfig>, Vec<ProviderConfig>) {
        // If the user supplied both structured sections, use them as-is.
        if !self.networks.is_empty() && !self.providers.is_empty() {
            return (self.networks.clone(), self.providers.clone());
        }

        // Synthesize defaults from legacy fields, then merge any partial
        // structured entries the user provided.
        let mut networks = HashMap::new();
        let mut providers = Vec::new();

        // Solana mainnet — always synthesized (has a default URL).
        networks.insert(
            "solana-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );
        providers.push(ProviderConfig {
            network: "solana-mainnet".to_string(),
            kind: "rpc".to_string(),
            url: self.solana_rpc_url.clone(),
            priority: Some(1),
            capabilities: vec!["historical".to_string(), "balances".to_string()],
            token_env: None,
            token: None,
            headers: None,
        });

        // Solana gRPC (optional).
        if let Some(ref grpc_url) = self.solana_grpc_url {
            providers.push(ProviderConfig {
                network: "solana-mainnet".to_string(),
                kind: "grpc".to_string(),
                url: grpc_url.clone(),
                priority: Some(1),
                capabilities: vec!["stream".to_string()],
                // Also check the env var as a fallback for tokens not set in TOML.
                token_env: Some("SOLANA_GRPC_TOKEN".to_string()),
                // Pass the resolved TOML value directly so it is not lost when
                // the raw env var is absent.
                token: self.solana_grpc_token.clone(),
                headers: None,
            });
        }

        // Ethereum mainnet — always synthesized (has a default URL).
        networks.insert(
            "ethereum-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );
        providers.push(ProviderConfig {
            network: "ethereum-mainnet".to_string(),
            kind: "rpc".to_string(),
            url: self.evm_rpc_url.clone(),
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
        });

        // EVM trace (optional).
        if self.evm_trace_enabled {
            let trace_url = self
                .evm_trace_rpc_url
                .clone()
                .unwrap_or_else(|| self.evm_rpc_url.clone());
            providers.push(ProviderConfig {
                network: "ethereum-mainnet".to_string(),
                kind: "trace".to_string(),
                url: trace_url,
                priority: Some(2),
                capabilities: vec!["debug_trace_transaction".to_string()],
                token_env: None,
                token: None,
                headers: None,
            });
        }

        // Hyperliquid mainnet — always synthesized.
        // The canonical network ID is "hypercore-mainnet" (matching the seeded
        // `networks` table and all existing Hyperliquid codepaths).
        networks.insert(
            "hypercore-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );
        providers.push(ProviderConfig {
            network: "hypercore-mainnet".to_string(),
            kind: "rest".to_string(),
            url: "https://api.hyperliquid.xyz".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: None,
        });
        providers.push(ProviderConfig {
            network: "hypercore-mainnet".to_string(),
            kind: "ws".to_string(),
            url: "wss://api.hyperliquid.xyz/ws".to_string(),
            priority: Some(1),
            capabilities: vec!["stream".to_string()],
            token_env: None,
            token: None,
            headers: None,
        });

        // Merge any user-supplied partial structured entries so that staged
        // migration (adding networks or providers incrementally) works.
        for (id, cfg) in &self.networks {
            networks.entry(id.clone()).or_insert_with(|| cfg.clone());
        }
        for p in &self.providers {
            // Avoid duplicating a provider that matches an already-synthesized
            // entry on (network, kind, url).
            let dominated = providers.iter().any(|existing| {
                existing.network == p.network && existing.kind == p.kind && existing.url == p.url
            });
            if !dominated {
                providers.push(p.clone());
            }
        }

        (networks, providers)
    }

    /// Build a `ProviderRegistry` from this config.
    ///
    /// Uses `effective_provider_config()` to merge legacy singleton fields
    /// with structured provider config, then constructs the registry.
    pub fn provider_registry(&self) -> Result<ProviderRegistry, crate::provider::RegistryError> {
        let (networks, providers) = self.effective_provider_config();
        ProviderRegistry::from_config(&networks, &providers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{NetworkId, ProviderCapability, ProviderKind};

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.port, 3000);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.pool_size, 10);
        assert_eq!(config.log_level, "info");
        assert_eq!(config.ingest_limit, 50);
        assert!(!config.evm_trace_enabled);
        assert!(config.evm_trace_rpc_url.is_none());
        assert!(config.networks.is_empty());
        assert!(config.providers.is_empty());
    }

    #[test]
    fn test_load_from_defaults_and_env() {
        // SAFETY: These env var mutations are unsafe since Rust 1.83 but
        // acceptable in sequential unit tests (cargo test runs #[test] fns
        // serially within a single test binary by default).
        unsafe {
            std::env::remove_var("SPECTRAPLEX_PORT");
            std::env::remove_var("SPECTRAPLEX_HOST");

            std::env::set_var("DATABASE_URL", "postgres://test:***@localhost/test");
            std::env::set_var("SPECTRAPLEX_PORT", "8080");
        }

        let config = AppConfig::load().unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.database_url, "postgres://test:***@localhost/test");
        assert_eq!(config.pool_size, 10);

        unsafe {
            std::env::remove_var("DATABASE_URL");
            std::env::remove_var("SPECTRAPLEX_PORT");
        }
    }

    #[test]
    fn test_allowed_wallets_set_none() {
        let config = AppConfig::default();
        assert!(config.allowed_wallets_set().is_none());
    }

    #[test]
    fn test_allowed_wallets_set_lowercases_evm_only() {
        let config = AppConfig {
            allowed_wallets: Some(
                "0xAbC123, DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy ,0XdeF456".to_string(),
            ),
            ..AppConfig::default()
        };
        let set = config.allowed_wallets_set().unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.contains("0xabc123"));
        assert!(set.contains("0xdef456"));
        assert!(set.contains("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy"));
        assert!(!set.contains("drpbcbmxvndk7mapm5tgv6mvb3v1srmc86pz8okm21hy"));
    }

    #[test]
    fn test_allowed_wallets_set_ignores_empty() {
        let config = AppConfig {
            allowed_wallets: Some("wallet1,,wallet2,".to_string()),
            ..AppConfig::default()
        };
        let set = config.allowed_wallets_set().unwrap();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_validate_empty_database_url() {
        let config = AppConfig::default();
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation(ref msg) if msg.contains("database_url")),
            "expected database_url validation error, got: {err}"
        );
    }

    #[test]
    fn test_validate_whitespace_database_url() {
        let config = AppConfig {
            database_url: "   ".to_string(),
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_zero_port() {
        let config = AppConfig {
            database_url: "postgres://localhost/test".to_string(),
            port: 0,
            ..AppConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation(ref msg) if msg.contains("port")),
            "expected port validation error, got: {err}"
        );
    }

    #[test]
    fn test_validate_zero_pool_size() {
        let config = AppConfig {
            database_url: "postgres://localhost/test".to_string(),
            pool_size: 0,
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_pool_size_too_large() {
        let config = AppConfig {
            database_url: "postgres://localhost/test".to_string(),
            pool_size: 1001,
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_valid_config() {
        let config = AppConfig {
            database_url: "postgres://localhost/test".to_string(),
            ..AppConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_pool_size_boundary() {
        let config = AppConfig {
            database_url: "postgres://localhost/test".to_string(),
            pool_size: 1,
            ..AppConfig::default()
        };
        assert!(config.validate().is_ok());

        let config = AppConfig {
            database_url: "postgres://localhost/test".to_string(),
            pool_size: 1000,
            ..AppConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_export_dir() {
        let config = AppConfig {
            database_url: "postgres://localhost/test".to_string(),
            export_dir: "".to_string(),
            ..AppConfig::default()
        };
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation(ref msg) if msg.contains("export_dir")),
            "expected export_dir validation error, got: {err}"
        );
    }

    #[test]
    fn test_validate_whitespace_export_dir() {
        let config = AppConfig {
            database_url: "postgres://localhost/test".to_string(),
            export_dir: "   ".to_string(),
            ..AppConfig::default()
        };
        assert!(config.validate().is_err());
    }

    // -- Backward compatibility: legacy fields -> synthesized providers --

    #[test]
    fn test_effective_config_from_legacy_defaults() {
        let config = AppConfig::default();
        let (networks, providers) = config.effective_provider_config();

        // Should synthesize solana-mainnet, ethereum-mainnet, hypercore-mainnet.
        assert_eq!(networks.len(), 3);
        assert!(networks.contains_key("solana-mainnet"));
        assert!(networks.contains_key("ethereum-mainnet"));
        assert!(networks.contains_key("hypercore-mainnet"));

        // Solana RPC + Ethereum RPC + Hyperliquid REST + Hyperliquid WS = 4.
        assert_eq!(providers.len(), 4);

        let sol_rpc = providers
            .iter()
            .find(|p| p.network == "solana-mainnet" && p.kind == "rpc");
        assert!(sol_rpc.is_some());
        assert_eq!(sol_rpc.unwrap().url, "https://api.mainnet-beta.solana.com");
    }

    #[test]
    fn test_effective_config_legacy_with_grpc() {
        let config = AppConfig {
            solana_grpc_url: Some("https://grpc.example.com".to_string()),
            solana_grpc_token: Some("secret".to_string()),
            ..AppConfig::default()
        };
        let (_, providers) = config.effective_provider_config();

        let sol_grpc = providers
            .iter()
            .find(|p| p.network == "solana-mainnet" && p.kind == "grpc");
        assert!(sol_grpc.is_some());
        let grpc = sol_grpc.unwrap();
        assert_eq!(grpc.url, "https://grpc.example.com");
        // The token should be passed through directly from the config value.
        assert_eq!(grpc.token.as_deref(), Some("secret"));
        // token_env is also set as a fallback.
        assert_eq!(grpc.token_env.as_deref(), Some("SOLANA_GRPC_TOKEN"));
    }

    #[test]
    fn test_effective_config_legacy_with_evm_trace() {
        let config = AppConfig {
            evm_trace_enabled: true,
            evm_trace_rpc_url: Some("https://eth-trace.alchemy.com".to_string()),
            ..AppConfig::default()
        };
        let (_, providers) = config.effective_provider_config();

        let eth_trace = providers
            .iter()
            .find(|p| p.network == "ethereum-mainnet" && p.kind == "trace");
        assert!(eth_trace.is_some());
        assert_eq!(eth_trace.unwrap().url, "https://eth-trace.alchemy.com");
    }

    #[test]
    fn test_effective_config_legacy_trace_uses_evm_rpc_as_fallback() {
        let config = AppConfig {
            evm_trace_enabled: true,
            evm_trace_rpc_url: None,
            evm_rpc_url: "https://my-node.example.com".to_string(),
            ..AppConfig::default()
        };
        let (_, providers) = config.effective_provider_config();

        let eth_trace = providers
            .iter()
            .find(|p| p.network == "ethereum-mainnet" && p.kind == "trace");
        assert!(eth_trace.is_some());
        assert_eq!(eth_trace.unwrap().url, "https://my-node.example.com");
    }

    #[test]
    fn test_effective_config_legacy_no_trace_when_disabled() {
        let config = AppConfig {
            evm_trace_enabled: false,
            evm_trace_rpc_url: Some("https://eth-trace.alchemy.com".to_string()),
            ..AppConfig::default()
        };
        let (_, providers) = config.effective_provider_config();

        let eth_trace = providers
            .iter()
            .find(|p| p.network == "ethereum-mainnet" && p.kind == "trace");
        assert!(eth_trace.is_none());
    }

    #[test]
    fn test_effective_config_structured_takes_precedence() {
        let mut networks = HashMap::new();
        networks.insert("base-mainnet".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "base-mainnet".to_string(),
            kind: "rpc".to_string(),
            url: "https://base.rpc".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let config = AppConfig {
            networks,
            providers,
            ..AppConfig::default()
        };
        let (nets, provs) = config.effective_provider_config();

        // Should return the explicit config, NOT synthesized legacy.
        assert_eq!(nets.len(), 1);
        assert!(nets.contains_key("base-mainnet"));
        assert_eq!(provs.len(), 1);
        assert_eq!(provs[0].network, "base-mainnet");
    }

    #[test]
    fn test_effective_config_partial_structured_still_synthesizes() {
        // Only networks supplied, no providers. Legacy synthesis should
        // still run AND the user's partial entries should be merged in.
        let mut networks = HashMap::new();
        networks.insert("base-mainnet".to_string(), NetworkConfig { enabled: true });

        let config = AppConfig {
            networks,
            ..AppConfig::default()
        };
        let (nets, provs) = config.effective_provider_config();

        // Legacy synthesis should produce its default providers.
        assert!(nets.contains_key("solana-mainnet"));
        assert!(nets.contains_key("ethereum-mainnet"));
        assert!(nets.contains_key("hypercore-mainnet"));
        assert!(!provs.is_empty());

        // The user's partial network entry should also be present.
        assert!(
            nets.contains_key("base-mainnet"),
            "user-supplied partial network entry must be merged"
        );
    }

    #[test]
    fn test_effective_config_partial_providers_merged() {
        // Only providers supplied, no networks. Legacy synthesis should
        // run AND the user's provider entry should be merged in.
        let user_provider = ProviderConfig {
            network: "base-mainnet".to_string(),
            kind: "rpc".to_string(),
            url: "https://base-rpc.example.com".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string(), "logs".to_string()],
            token_env: None,
            token: None,
            headers: None,
        };

        let config = AppConfig {
            providers: vec![user_provider],
            ..AppConfig::default()
        };
        let (nets, provs) = config.effective_provider_config();

        // Legacy defaults present.
        assert!(nets.contains_key("solana-mainnet"));
        assert!(nets.contains_key("ethereum-mainnet"));

        // User's provider entry is merged in.
        let base_prov = provs
            .iter()
            .find(|p| p.network == "base-mainnet")
            .expect("user-supplied provider must be merged");
        assert_eq!(base_prov.url, "https://base-rpc.example.com");
    }

    #[test]
    fn test_provider_registry_from_default_config() {
        let config = AppConfig::default();
        let registry = config.provider_registry().unwrap();

        let sol = NetworkId::new("solana-mainnet");
        let eth = NetworkId::new("ethereum-mainnet");
        let hl = NetworkId::new("hypercore-mainnet");

        assert!(registry.is_network_enabled(&sol));
        assert!(registry.is_network_enabled(&eth));
        assert!(registry.is_network_enabled(&hl));

        // Solana historical should resolve.
        let p = registry
            .resolve(&sol, ProviderCapability::Historical)
            .unwrap();
        assert_eq!(p.kind, ProviderKind::Rpc);

        // Ethereum logs should resolve.
        let p = registry.resolve(&eth, ProviderCapability::Logs).unwrap();
        assert_eq!(p.kind, ProviderKind::Rpc);

        // Hyperliquid historical should resolve.
        let p = registry
            .resolve(&hl, ProviderCapability::Historical)
            .unwrap();
        assert_eq!(p.kind, ProviderKind::Rest);

        // Hyperliquid stream should resolve.
        let p = registry.resolve(&hl, ProviderCapability::Stream).unwrap();
        assert_eq!(p.kind, ProviderKind::Ws);
    }

    #[test]
    fn test_provider_registry_from_structured_config() {
        let mut networks = HashMap::new();
        networks.insert("base-mainnet".to_string(), NetworkConfig { enabled: true });
        networks.insert(
            "arbitrum-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );

        let providers = vec![
            ProviderConfig {
                network: "base-mainnet".to_string(),
                kind: "rpc".to_string(),
                url: "https://base.rpc".to_string(),
                priority: Some(1),
                capabilities: vec![
                    "historical".to_string(),
                    "logs".to_string(),
                    "receipts".to_string(),
                ],
                token_env: None,
                token: None,
                headers: None,
            },
            ProviderConfig {
                network: "arbitrum-mainnet".to_string(),
                kind: "rpc".to_string(),
                url: "https://arb.rpc".to_string(),
                priority: Some(1),
                capabilities: vec![
                    "historical".to_string(),
                    "logs".to_string(),
                    "receipts".to_string(),
                ],
                token_env: None,
                token: None,
                headers: None,
            },
        ];

        let config = AppConfig {
            networks,
            providers,
            ..AppConfig::default()
        };

        let registry = config.provider_registry().unwrap();
        let base = NetworkId::new("base-mainnet");
        let arb = NetworkId::new("arbitrum-mainnet");

        assert!(registry.is_network_enabled(&base));
        assert!(registry.is_network_enabled(&arb));

        let p = registry
            .resolve(&base, ProviderCapability::Historical)
            .unwrap();
        assert_eq!(p.url, "https://base.rpc");

        let p = registry.resolve(&arb, ProviderCapability::Logs).unwrap();
        assert_eq!(p.url, "https://arb.rpc");
    }
}
