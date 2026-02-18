use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub port: u16,
    pub host: String,
    pub database_url: String,
    pub pool_size: u32,
    pub log_level: String,
    pub ingest_limit: usize,
    pub solana_rpc_url: String,
    pub evm_rpc_url: String,
    pub api_key: Option<String>,
    pub allowed_wallets: Option<String>,
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
            solana_rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            evm_rpc_url: "https://eth.llamarpc.com".to_string(),
            api_key: None,
            allowed_wallets: None,
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Serialized::defaults(AppConfig::default()))
            .merge(Toml::file("spectraplex.toml"))
            .merge(Env::prefixed("SPECTRAPLEX_"))
            .merge(Env::raw().only(&["DATABASE_URL", "SOLANA_RPC_URL", "EVM_RPC_URL"]))
            .extract()
            .map_err(Box::new)
    }

    pub fn allowed_wallets_set(&self) -> Option<HashSet<String>> {
        self.allowed_wallets.as_ref().map(|s| {
            s.split(',')
                .map(|w| w.trim().to_lowercase())
                .filter(|w| !w.is_empty())
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.port, 3000);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.pool_size, 10);
        assert_eq!(config.log_level, "info");
        assert_eq!(config.ingest_limit, 50);
    }

    #[test]
    fn test_load_from_defaults_and_env() {
        // Clear any pre-existing env vars that might interfere
        std::env::remove_var("SPECTRAPLEX_PORT");
        std::env::remove_var("SPECTRAPLEX_HOST");

        std::env::set_var("DATABASE_URL", "postgres://test:test@localhost/test");
        std::env::set_var("SPECTRAPLEX_PORT", "8080");

        let config = AppConfig::load().unwrap();
        assert_eq!(config.port, 8080);
        assert_eq!(config.database_url, "postgres://test:test@localhost/test");
        assert_eq!(config.pool_size, 10); // default

        std::env::remove_var("DATABASE_URL");
        std::env::remove_var("SPECTRAPLEX_PORT");
    }

    #[test]
    fn test_allowed_wallets_set_none() {
        let config = AppConfig::default();
        assert!(config.allowed_wallets_set().is_none());
    }

    #[test]
    fn test_allowed_wallets_set_parses_csv() {
        let config = AppConfig {
            allowed_wallets: Some("Wallet1, wallet2 ,WALLET3".to_string()),
            ..AppConfig::default()
        };
        let set = config.allowed_wallets_set().unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.contains("wallet1"));
        assert!(set.contains("wallet2"));
        assert!(set.contains("wallet3"));
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
}
