use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub port: u16,
    pub host: String,
    pub database_url: String,
    pub pool_size: u32,
    pub log_level: String,
    pub ingest_limit: usize,
    pub solana_rpc_url: String,
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
        }
    }
}

impl AppConfig {
    pub fn load() -> Result<Self, Box<figment::Error>> {
        Figment::new()
            .merge(Serialized::defaults(AppConfig::default()))
            .merge(Toml::file("spectraplex.toml"))
            .merge(Env::prefixed("SPECTRAPLEX_"))
            .merge(Env::raw().only(&["DATABASE_URL", "SOLANA_RPC_URL"]))
            .extract()
            .map_err(Box::new)
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
}
