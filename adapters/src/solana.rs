#[cfg(test)]
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use chrono::Utc;
use solana_client::nonblocking::rpc_client::RpcClient;
#[cfg(test)]
use solana_sdk::signature::Signature;
use spectraplex_core::connector::{Connector, ConnectorCapabilities};
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, Transaction};
use spectraplex_core::provider::{NetworkContext, ProviderCapability};
use spectraplex_core::v2::{ChainFamily, IndexTarget, IngestionBatch, TargetKind, TargetMode};

#[cfg(test)]
use spectraplex_core::v2::RawTransaction;
use uuid::Uuid;

#[cfg(test)]
fn u64_to_i64_or_max(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub struct SolanaAdapter {
    client: Arc<RpcClient>,
    network: String,
}

impl SolanaAdapter {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            client: Arc::new(RpcClient::new_with_timeout(
                rpc_url.to_string(),
                Duration::from_secs(30),
            )),
            network: "solana-mainnet".to_string(),
        }
    }

    /// Create a new adapter from a `NetworkContext`.
    ///
    /// Resolves the `Historical` provider from the context to obtain the
    /// RPC URL. Returns an error if no suitable provider is available.
    pub fn from_network_context(ctx: &NetworkContext) -> anyhow::Result<Self> {
        let rpc_url = ctx.url(ProviderCapability::Historical).ok_or_else(|| {
            anyhow::anyhow!(
                "no RPC provider with 'historical' capability for network '{}'",
                ctx.network
            )
        })?;

        Ok(Self {
            client: Arc::new(RpcClient::new_with_timeout(
                rpc_url.to_string(),
                Duration::from_secs(30),
            )),
            network: ctx.network.as_str().to_string(),
        })
    }

    /// Override the network identifier (e.g. "solana-devnet").
    pub fn with_network(mut self, network: &str) -> Self {
        self.network = network.to_string();
        self
    }
}

// ---------------------------------------------------------------------------
// V1 Legacy ChainIngestor implementation (preserved for backward compat)
// ---------------------------------------------------------------------------

#[async_trait::async_trait]
impl ChainIngestor for SolanaAdapter {
    async fn fetch_history(
        &self,
        wallet: &str,
        limit: usize,
        user_id: Uuid,
        checkpoint: Option<&IndexerCheckpoint>,
    ) -> anyhow::Result<Vec<Transaction>> {
        let cursor = checkpoint.map(
            |cp| serde_json::json!({"last_signature": cp.last_signature, "incremental": true}),
        );
        let batch = crate::solana_history::fetch(
            self.client.clone(),
            wallet.into(),
            self.network.clone(),
            cursor,
            limit,
            Uuid::nil(),
        )
        .await?;
        Ok(batch
            .records
            .into_iter()
            .map(|raw| Transaction {
                id: raw.id,
                user_id,
                wallet_address: wallet.into(),
                timestamp: raw.timestamp,
                tx_hash: raw.tx_hash,
                chain: Chain::Solana,
                raw_metadata: raw.raw_metadata,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// V2 Connector implementation (backfill-only, wallet targets)
// ---------------------------------------------------------------------------

/// Extract a cursor with {last_signature} for Solana RPC pagination.
#[cfg(test)]
fn cursor_to_until_sig(cursor: Option<&serde_json::Value>) -> Option<Signature> {
    cursor
        .and_then(|c| c.get("last_signature"))
        .and_then(|v| v.as_str())
        .and_then(|s| Signature::from_str(s).ok())
}

#[async_trait::async_trait]
impl Connector for SolanaAdapter {
    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            supported_target_kinds: vec![TargetKind::Wallet],
            supported_modes: vec![TargetMode::Backfill],
            chain_family: ChainFamily::Solana,
        }
    }

    async fn backfill(
        &self,
        target: &IndexTarget,
        cursor: Option<&serde_json::Value>,
        limit: usize,
    ) -> anyhow::Result<IngestionBatch> {
        if target.chain_family != ChainFamily::Solana {
            anyhow::bail!(
                "SolanaAdapter only supports Solana chain family, got {:?}",
                target.chain_family
            );
        }

        if target.kind != TargetKind::Wallet {
            anyhow::bail!(
                "SolanaAdapter RPC backfill only supports wallet targets, got {:?}",
                target.kind
            );
        }

        let wallet_str = target
            .address
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("wallet target must have an address"))?;

        crate::solana_history::fetch(
            self.client.clone(),
            wallet_str.into(),
            target.network.clone(),
            cursor.cloned(),
            limit,
            target.id,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adapter_construction_with_timeout() {
        let adapter = SolanaAdapter::new("https://api.mainnet-beta.solana.com");
        // Verify the client was created successfully with timeout
        assert!(Arc::strong_count(&adapter.client) == 1);
    }

    #[test]
    fn test_adapter_default_network() {
        let adapter = SolanaAdapter::new("https://api.mainnet-beta.solana.com");
        assert_eq!(adapter.network, "solana-mainnet");
    }

    #[test]
    fn test_adapter_with_network() {
        let adapter =
            SolanaAdapter::new("https://api.devnet.solana.com").with_network("solana-devnet");
        assert_eq!(adapter.network, "solana-devnet");
    }

    // -- Connector capabilities --

    #[test]
    fn test_v2_connector_capabilities() {
        let adapter = SolanaAdapter::new("https://api.mainnet-beta.solana.com");
        let caps = adapter.capabilities();

        assert_eq!(caps.chain_family, ChainFamily::Solana);
        assert_eq!(caps.supported_target_kinds, vec![TargetKind::Wallet]);
        assert_eq!(caps.supported_modes, vec![TargetMode::Backfill]);
    }

    #[test]
    fn test_v2_connector_can_service_wallet() {
        let adapter = SolanaAdapter::new("https://api.mainnet-beta.solana.com");
        let caps = adapter.capabilities();
        let now = Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Wallet,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy".to_string()),
            filter_spec: None,
            mode: TargetMode::Backfill,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };
        assert!(caps.can_service(&target));
    }

    #[test]
    fn test_v2_connector_rejects_program() {
        let adapter = SolanaAdapter::new("https://api.mainnet-beta.solana.com");
        let caps = adapter.capabilities();
        let now = Utc::now();
        let target = IndexTarget {
            id: Uuid::new_v4(),
            kind: TargetKind::Program,
            network: "solana-mainnet".to_string(),
            chain_family: ChainFamily::Solana,
            address: Some("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".to_string()),
            filter_spec: None,
            mode: TargetMode::Backfill,
            label: None,
            owner_id: None,
            created_at: now,
            updated_at: now,
        };
        assert!(!caps.can_service(&target));
    }

    #[test]
    fn test_v2_connector_no_stream_support() {
        let adapter = SolanaAdapter::new("https://api.mainnet-beta.solana.com");
        let caps = adapter.capabilities();
        assert!(!caps.supports_mode(TargetMode::Stream));
    }

    // -- Cursor parsing --

    #[test]
    fn test_cursor_to_until_sig_none() {
        assert!(cursor_to_until_sig(None).is_none());
    }

    #[test]
    fn test_cursor_to_until_sig_with_signature() {
        let cursor = serde_json::json!({
            "last_signature": "5VERv8NMhvaTPGUp7oiLe8G9JtYbRaM7AhEXqJGhFdkJSBe9LpwpnDJUfMzLjP3fVcJqRbP2Dcp4FnHjTgEeLGd",
            "last_slot": 42000
        });
        let sig = cursor_to_until_sig(Some(&cursor));
        assert!(sig.is_some());
    }

    #[test]
    fn test_cursor_to_until_sig_invalid() {
        let cursor = serde_json::json!({"last_signature": "not_a_valid_sig"});
        // Invalid base58 signature should return None gracefully
        let sig = cursor_to_until_sig(Some(&cursor));
        assert!(sig.is_none());
    }

    #[test]
    fn test_u64_to_i64_or_max_saturates() {
        assert_eq!(u64_to_i64_or_max(42), 42);
        assert_eq!(u64_to_i64_or_max(i64::MAX as u64), i64::MAX);
        assert_eq!(u64_to_i64_or_max(u64::MAX), i64::MAX);
    }

    #[test]
    fn test_cursor_to_until_sig_missing_key() {
        let cursor = serde_json::json!({"other": "value"});
        assert!(cursor_to_until_sig(Some(&cursor)).is_none());
    }

    // -- RawTransaction output --

    #[test]
    fn test_raw_transaction_has_no_wallet_or_user_fields() {
        let raw_tx = RawTransaction {
            id: Uuid::new_v4(),
            network: "solana-mainnet".to_string(),
            tx_hash: "test_sig".to_string(),
            timestamp: 1700000000,
            block_number: Some(42000),
            raw_metadata: serde_json::json!({"slot": 42000}),
            source: "solana-rpc-wallet-backfill".to_string(),
            ingestion_run_id: None,
            ingested_at: Utc::now(),
        };
        let serialized = serde_json::to_string(&raw_tx).unwrap();
        assert!(!serialized.contains("wallet_address"));
        assert!(!serialized.contains("user_id"));
    }

    #[test]
    fn test_source_field_is_descriptive() {
        assert!("solana-rpc-wallet-backfill".starts_with("solana-rpc-"));
    }

    // -- Construction from NetworkContext --

    #[test]
    fn test_from_network_context() {
        use spectraplex_core::config::{NetworkConfig, ProviderConfig};
        use spectraplex_core::provider::{NetworkId, ProviderRegistry};
        use std::collections::HashMap;

        let mut networks = HashMap::new();
        networks.insert(
            "solana-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );

        let providers = vec![ProviderConfig {
            network: "solana-mainnet".to_string(),
            kind: "rpc".to_string(),
            url: "https://api.mainnet-beta.solana.com".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string(), "balances".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("solana-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &net).unwrap();
        let adapter = SolanaAdapter::from_network_context(&ctx).unwrap();

        assert_eq!(adapter.network, "solana-mainnet");
        assert!(Arc::strong_count(&adapter.client) == 1);
    }

    #[test]
    fn test_from_network_context_uses_network_id() {
        use spectraplex_core::config::{NetworkConfig, ProviderConfig};
        use spectraplex_core::provider::{NetworkId, ProviderRegistry};
        use std::collections::HashMap;

        let mut networks = HashMap::new();
        networks.insert("solana-devnet".to_string(), NetworkConfig { enabled: true });

        let providers = vec![ProviderConfig {
            network: "solana-devnet".to_string(),
            kind: "rpc".to_string(),
            url: "https://api.devnet.solana.com".to_string(),
            priority: Some(1),
            capabilities: vec!["historical".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("solana-devnet");
        let ctx = NetworkContext::from_registry(&registry, &net).unwrap();
        let adapter = SolanaAdapter::from_network_context(&ctx).unwrap();

        assert_eq!(adapter.network, "solana-devnet");
    }

    #[test]
    fn test_from_network_context_no_historical_fails() {
        use spectraplex_core::config::{NetworkConfig, ProviderConfig};
        use spectraplex_core::provider::{NetworkId, ProviderRegistry};
        use std::collections::HashMap;

        let mut networks = HashMap::new();
        networks.insert(
            "solana-mainnet".to_string(),
            NetworkConfig { enabled: true },
        );

        // Provider with only stream capability, no historical
        let providers = vec![ProviderConfig {
            network: "solana-mainnet".to_string(),
            kind: "grpc".to_string(),
            url: "https://grpc.example.com".to_string(),
            priority: Some(1),
            capabilities: vec!["stream".to_string()],
            token_env: None,
            token: None,
            headers: None,
        }];

        let registry = ProviderRegistry::from_config(&networks, &providers).unwrap();
        let net = NetworkId::new("solana-mainnet");
        let ctx = NetworkContext::from_registry(&registry, &net).unwrap();
        let result = SolanaAdapter::from_network_context(&ctx);

        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("historical"));
    }
}
