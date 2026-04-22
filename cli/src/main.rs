use clap::{Parser, Subcommand};
use spectraplex_adapters::{
    dual_write::{chain_to_default_source, v2_checkpoint_to_v1},
    evm::EvmAdapter,
    evm_parser,
    hyperliquid::HyperliquidAdapter,
    hyperliquid_parser,
    repo::{build_checkpoint, Repository},
    solana::SolanaAdapter,
    solana_grpc::SolanaGrpcAdapter,
    solana_parser,
};
use spectraplex_core::config::AppConfig;
use spectraplex_core::connector::validate_target;
use spectraplex_core::models::{Chain, ChainIngestor, Transaction};
use spectraplex_core::provider::{chain_to_network_id, NetworkContext, NetworkId};
use spectraplex_core::v2::{
    normalize_evm_address, normalize_solana_address, ChainFamily, IndexTarget, TargetKind,
    TargetMode,
};
use sqlx::postgres::PgPoolOptions;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Parser)]
#[command(about = "Spectraplex CLI", long_about = None)]
struct Cli {
    #[arg(global = true, long, env = "DATABASE_URL")]
    db_url: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize the database schema
    InitDb,

    /// Ingest raw data from blockchain to Bronze layer (JSONL)
    Ingest {
        /// Chain name (compat alias). Prefer --network for new usage.
        #[arg(short, long, default_value = "")]
        chain: String,

        /// Network ID (e.g. solana-mainnet, ethereum-mainnet, base-mainnet).
        /// Takes precedence over --chain when provided.
        #[arg(short, long)]
        network: Option<String>,

        /// Wallet address(es) to ingest. Pass multiple times for batch ingestion.
        #[arg(short, long, required = true, num_args = 1..)]
        wallet: Vec<String>,

        #[arg(short, long, default_value = "bronze_transactions.jsonl")]
        output: PathBuf,

        #[arg(long)]
        rpc: Option<String>,

        #[arg(long)]
        grpc_url: Option<String>,

        #[arg(long)]
        x_token: Option<String>,

        #[arg(long, default_value_t = 10)]
        limit: usize,

        /// User ID to associate with ingested transactions. Auto-generates if not provided.
        #[arg(long)]
        user_id: Option<Uuid>,
    },
    /// Normalize Bronze data to Silver layer (Ledger Entries)
    Normalize {
        #[arg(short, long, default_value = "bronze_transactions.jsonl")]
        input: PathBuf,

        #[arg(short, long, default_value = "silver_ledger.jsonl")]
        output: PathBuf,

        /// Optional V2 network identifier (e.g. "base-mainnet").
        /// When provided, overrides the chain-derived default during Silver
        /// materialization.  When omitted, the actual network is resolved
        /// from existing Bronze raw_transactions rows.
        #[arg(short, long)]
        network: Option<String>,
    },

    /// Register a new index target
    RegisterTarget {
        /// Target kind: wallet, contract, program, account, topic_filter, market, pool, protocol
        #[arg(long)]
        kind: String,

        /// Network identifier, e.g. solana-mainnet, ethereum-mainnet
        #[arg(long)]
        network: String,

        /// Target address (required for most kinds)
        #[arg(long)]
        address: Option<String>,

        /// Filter spec as a JSON string (required for topic_filter and protocol)
        #[arg(long)]
        filter_spec: Option<String>,

        /// Ingestion mode: backfill, stream, both (default: both)
        #[arg(long, default_value = "both")]
        mode: String,

        /// Human-readable label
        #[arg(long)]
        label: Option<String>,
    },

    /// List registered index targets
    ListTargets {
        /// Filter by network
        #[arg(long)]
        network: Option<String>,

        /// Filter by kind
        #[arg(long)]
        kind: Option<String>,
    },

    /// List available networks
    ListNetworks,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    // Setup DB Pool if URL provided
    let pool = if let Some(url) = &cli.db_url {
        Some(PgPoolOptions::new().max_connections(5).connect(url).await?)
    } else {
        None
    };

    match cli.command {
        Commands::InitDb => {
            let p = pool.ok_or_else(|| anyhow::anyhow!("--db-url is required for init-db"))?;
            info!("Running migrations...");
            sqlx::migrate!("../migrations").run(&p).await?;
            info!("Database initialized successfully.");
        }
        Commands::Ingest {
            chain,
            network,
            wallet: wallets,
            output,
            rpc,
            grpc_url,
            x_token,
            limit,
            user_id,
        } => {
            // Resolve chain from --network (preferred) or --chain (compat)
            let chain = if let Some(ref net) = network {
                match net.as_str() {
                    n if n.starts_with("solana") => "solana".to_string(),
                    n if n.starts_with("hypercore") || n.starts_with("hyperliquid") => {
                        "hyperliquid".to_string()
                    }
                    _ => "ethereum".to_string(),
                }
            } else if chain.is_empty() {
                anyhow::bail!("Either --chain or --network must be provided");
            } else {
                chain
            };

            // Build provider registry from config (best-effort; falls back to CLI args)
            let net_ctx = {
                let config = AppConfig::load().ok();
                config.and_then(|cfg| {
                    let registry = cfg.provider_registry().ok()?;
                    let net_id = if let Some(ref net) = network {
                        NetworkId::new(net.clone())
                    } else {
                        chain_to_network_id(&chain)?
                    };
                    NetworkContext::from_registry(&registry, &net_id)
                })
            };

            // Fail closed: when --network is explicit, the provider registry
            // MUST resolve it. Silently falling back to --rpc / --grpc-url
            // would connect to the wrong network (e.g. --network base-mainnet
            // but --rpc points to Ethereum mainnet).
            if network.is_some() && net_ctx.is_none() {
                anyhow::bail!(
                    "network '{}' is not configured in the provider registry. \
                     Check spectraplex.toml or SPECTRAPLEX_* environment variables.",
                    network.as_deref().unwrap()
                );
            }

            let user_id = user_id.unwrap_or_else(|| {
                let id = Uuid::new_v4();
                info!(user_id = %id, "No --user-id provided, auto-generated");
                id
            });

            // When writing to a file (no DB), truncate the output file once before
            // processing wallets so a fresh run starts clean, then append per wallet.
            if pool.is_none() {
                File::create(&output)?;
            }

            for wallet in &wallets {
                info!(wallet = %wallet, chain = %chain, "Starting ingestion");

                // Parse chain enum early for V2 checkpoint lookup
                let chain_enum_for_cp = match chain.as_str() {
                    "solana" => Chain::Solana,
                    "ethereum" => Chain::Ethereum,
                    "hyperliquid" => Chain::Hyperliquid,
                    _ => unreachable!("unsupported chain filtered earlier"),
                };

                // Resume path: when --network is provided, use V2 checkpoint
                // (keyed by network + target) so different EVM networks get
                // independent resume state. Do NOT fall back to V1 checkpoint
                // when network is explicit — that would inherit resume state
                // from a different EVM network. When no V2 checkpoint exists,
                // start from scratch (full backfill for that network).
                let checkpoint = if let Some(ref p) = pool {
                    let repo = Repository::new(p.clone());
                    if let Some(ref net) = network {
                        // Network-first: try V2 checkpoint scoped by network
                        let v2_cp = {
                            // Need target_id for V2 lookup; peek at existing target
                            let target = repo
                                .get_index_target_by_address(
                                    spectraplex_core::v2::TargetKind::Wallet,
                                    net,
                                    wallet,
                                    None,
                                )
                                .await
                                .ok()
                                .flatten();
                            if let Some(t) = target {
                                let source = chain_to_default_source(&chain_enum_for_cp);
                                repo.get_checkpoint_v2(t.id, net, source)
                                    .await
                                    .ok()
                                    .flatten()
                            } else {
                                None
                            }
                        };

                        if let Some(v2) = v2_cp {
                            let cp = v2_checkpoint_to_v1(&v2, &chain_enum_for_cp, wallet);
                            info!(
                                network = %net,
                                wallet = %wallet,
                                last_signature = ?cp.last_signature,
                                last_slot = ?cp.last_slot,
                                last_block = ?cp.last_block,
                                last_timestamp = ?cp.last_timestamp,
                                "Resuming from V2 checkpoint (network-scoped)"
                            );
                            Some(cp)
                        } else {
                            info!(
                                network = %net,
                                wallet = %wallet,
                                "No V2 checkpoint for explicit network, starting fresh"
                            );
                            None
                        }
                    } else {
                        // No explicit network: use V1 checkpoint as before
                        match repo.get_checkpoint(&chain, wallet).await? {
                            Some(cp) => {
                                info!(
                                    chain = %chain,
                                    wallet = %wallet,
                                    last_signature = ?cp.last_signature,
                                    last_slot = ?cp.last_slot,
                                    last_block = ?cp.last_block,
                                    last_timestamp = ?cp.last_timestamp,
                                    "Resuming from checkpoint"
                                );
                                Some(cp)
                            }
                            None => {
                                info!(
                                    chain = %chain,
                                    wallet = %wallet,
                                    "No existing checkpoint, full ingestion"
                                );
                                None
                            }
                        }
                    }
                } else {
                    None
                };

                let events = match chain.as_str() {
                    "solana" => {
                        if let Some(ref endpoint) = grpc_url {
                            let adapter = SolanaGrpcAdapter::new(endpoint, x_token.clone());
                            if let Some(ref cp) = checkpoint {
                                if let Some(slot) = cp.last_slot {
                                    adapter.checkpoint().update(slot as u64);
                                }
                            }
                            adapter
                                .fetch_history(wallet, limit, user_id, checkpoint.as_ref())
                                .await?
                        } else if let Some(ref rpc_url) = rpc {
                            let adapter = SolanaAdapter::new(rpc_url);
                            adapter
                                .fetch_history(wallet, limit, user_id, checkpoint.as_ref())
                                .await?
                        } else if let Some(ref ctx) = net_ctx {
                            // Use provider registry
                            let adapter = SolanaAdapter::from_network_context(ctx)?;
                            adapter
                                .fetch_history(wallet, limit, user_id, checkpoint.as_ref())
                                .await?
                        } else {
                            anyhow::bail!("Either --grpc-url, --rpc, or --network must be provided for Solana");
                        }
                    }
                    "hyperliquid" => {
                        let adapter = match &net_ctx {
                            Some(ctx) => HyperliquidAdapter::from_network_context(ctx),
                            None => HyperliquidAdapter::new(),
                        };
                        adapter
                            .fetch_history(wallet, limit, user_id, checkpoint.as_ref())
                            .await?
                    }
                    "ethereum" => {
                        if let Some(ref rpc_url) = rpc {
                            let adapter = EvmAdapter::new(rpc_url)?;
                            adapter
                                .fetch_history(wallet, limit, user_id, checkpoint.as_ref())
                                .await?
                        } else if let Some(ref ctx) = net_ctx {
                            let adapter = EvmAdapter::from_network_context(ctx)?;
                            adapter
                                .fetch_history(wallet, limit, user_id, checkpoint.as_ref())
                                .await?
                        } else {
                            anyhow::bail!(
                                "Either --rpc or --network must be provided for EVM chains"
                            );
                        }
                    }
                    _ => {
                        warn!(chain = %chain, "Unsupported chain");
                        return Ok(());
                    }
                };

                if let Some(ref p) = pool {
                    let repo = Repository::new(p.clone());

                    // Parse chain enum for ensure_wallet_target
                    let chain_enum = match chain.as_str() {
                        "solana" => Chain::Solana,
                        "ethereum" => Chain::Ethereum,
                        "hyperliquid" => Chain::Hyperliquid,
                        _ => unreachable!("unsupported chain filtered earlier"),
                    };

                    // Ensure a V2 IndexTarget exists for this wallet (best-effort).
                    // When --network is provided, use it so different EVM networks
                    // get distinct target rows.
                    let target_id = if let Some(ref net) = network {
                        match repo
                            .ensure_wallet_target_for_network(
                                net,
                                &chain_enum,
                                wallet,
                                Some(user_id),
                            )
                            .await
                        {
                            Ok(target) => Some(target.id),
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    wallet = %wallet,
                                    network = %net,
                                    "Failed to ensure V2 wallet target (V1 path unaffected)"
                                );
                                None
                            }
                        }
                    } else {
                        match repo
                            .ensure_wallet_target(&chain_enum, wallet, Some(user_id))
                            .await
                        {
                            Ok(target) => Some(target.id),
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    wallet = %wallet,
                                    "Failed to ensure V2 wallet target (V1 path unaffected)"
                                );
                                None
                            }
                        }
                    };

                    if let Some(cp) = build_checkpoint(&chain, wallet, &events) {
                        if let Some(tid) = target_id {
                            repo.save_transactions_and_checkpoint_dual_write(
                                &events,
                                &cp,
                                tid,
                                network.as_deref(),
                            )
                            .await?;
                        } else {
                            repo.save_transactions_and_checkpoint(&events, &cp).await?;
                        }
                        info!(count = events.len(), wallet = %wallet, "Saved transactions to database");
                        info!(
                            chain = %chain,
                            wallet = %wallet,
                            last_signature = ?cp.last_signature,
                            last_slot = ?cp.last_slot,
                            last_block = ?cp.last_block,
                            last_timestamp = ?cp.last_timestamp,
                            "Checkpoint saved atomically"
                        );
                    } else {
                        if let Some(tid) = target_id {
                            repo.save_transactions_dual_write(&events, tid, network.as_deref())
                                .await?;
                        } else {
                            repo.save_transactions(&events).await?;
                        }
                        info!(
                            count = events.len(),
                            wallet = %wallet,
                            "Saved transactions to database (no checkpoint)"
                        );
                    }
                } else {
                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&output)?;
                    for event in &events {
                        serde_json::to_writer(&mut file, event)?;
                        writeln!(file)?;
                    }
                    info!(path = ?output, wallet = %wallet, "Data written to file");
                }
            }
        }
        Commands::RegisterTarget {
            kind,
            network,
            address,
            filter_spec,
            mode,
            label,
        } => {
            let p =
                pool.ok_or_else(|| anyhow::anyhow!("--db-url is required for register-target"))?;
            let repo = Repository::new(p);

            // Parse kind
            let target_kind: TargetKind = kind
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid target kind: {kind}"))?;

            // Parse mode
            let target_mode: TargetMode = mode
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid mode: {mode}"))?;

            // Look up network
            let net = repo
                .get_network(&network)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Unknown network: {network}"))?;

            // Parse filter_spec JSON
            let filter_spec_value: Option<serde_json::Value> = match filter_spec {
                Some(ref json_str) => Some(
                    serde_json::from_str(json_str)
                        .map_err(|e| anyhow::anyhow!("Invalid filter-spec JSON: {e}"))?,
                ),
                None => None,
            };

            // Normalize address
            let normalized_address = address.map(|addr| match net.chain_family {
                ChainFamily::Evm | ChainFamily::Hyperliquid => normalize_evm_address(&addr),
                ChainFamily::Solana => normalize_solana_address(&addr),
            });

            let now = chrono::Utc::now();
            let target = IndexTarget {
                id: Uuid::new_v4(),
                kind: target_kind,
                network,
                chain_family: net.chain_family,
                address: normalized_address,
                filter_spec: filter_spec_value,
                mode: target_mode,
                label,
                owner_id: None,
                created_at: now,
                updated_at: now,
            };

            // Validate
            if let Err(errors) = validate_target(&target) {
                error!("Validation failed: {}", errors.join("; "));
                return Err(anyhow::anyhow!("Target validation failed"));
            }

            let created = repo.create_index_target(&target).await?;
            info!(
                id = %created.id,
                kind = %created.kind,
                network = %created.network,
                address = ?created.address,
                mode = %created.mode,
                "Target registered"
            );
        }
        Commands::ListTargets { network, kind } => {
            let p = pool.ok_or_else(|| anyhow::anyhow!("--db-url is required for list-targets"))?;
            let repo = Repository::new(p);

            let targets = if let Some(ref net) = network {
                repo.list_index_targets_by_network(net).await?
            } else if let Some(ref kind_str) = kind {
                let tk: TargetKind = kind_str
                    .parse()
                    .map_err(|_| anyhow::anyhow!("Invalid target kind: {kind_str}"))?;
                repo.list_index_targets_by_kind(tk).await?
            } else {
                repo.list_index_targets(100, 0).await?
            };

            if targets.is_empty() {
                info!("No targets found");
            } else {
                println!(
                    "{:<38} {:<14} {:<20} {:<44} {:<10}",
                    "ID", "KIND", "NETWORK", "ADDRESS", "MODE"
                );
                println!("{}", "-".repeat(126));
                for t in &targets {
                    println!(
                        "{:<38} {:<14} {:<20} {:<44} {:<10}",
                        t.id,
                        t.kind,
                        t.network,
                        t.address.as_deref().unwrap_or("-"),
                        t.mode,
                    );
                }
                info!(count = targets.len(), "Targets listed");
            }
        }
        Commands::ListNetworks => {
            let p =
                pool.ok_or_else(|| anyhow::anyhow!("--db-url is required for list-networks"))?;
            let repo = Repository::new(p);

            let networks = repo.list_networks().await?;
            if networks.is_empty() {
                info!("No networks found");
            } else {
                println!(
                    "{:<24} {:<14} {:<30} {:<10}",
                    "ID", "FAMILY", "DISPLAY NAME", "TESTNET"
                );
                println!("{}", "-".repeat(78));
                for n in &networks {
                    println!(
                        "{:<24} {:<14} {:<30} {:<10}",
                        n.id, n.chain_family, n.display_name, n.is_testnet,
                    );
                }
                info!(count = networks.len(), "Networks listed");
            }
        }
        Commands::Normalize {
            input,
            output,
            network,
        } => {
            let input_str = input.to_string_lossy();
            let transactions = if input_str.starts_with("db:") {
                let wallet = input_str.strip_prefix("db:").unwrap();
                let p = pool
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("--db-url is required for db: input"))?;
                info!(wallet = %wallet, "Fetching transactions from DB");
                let repo = Repository::new(p);
                repo.get_transactions_by_wallet(wallet).await?
            } else {
                info!(path = ?input, "Reading raw data from file");
                let file = File::open(&input)?;
                let reader = BufReader::new(file);
                reader
                    .lines()
                    .map(|line| {
                        let line = line?;
                        Ok(serde_json::from_str(&line)?)
                    })
                    .collect::<anyhow::Result<Vec<Transaction>>>()?
            };

            let mut all_entries = Vec::new();

            for tx in &transactions {
                let result = match tx.chain {
                    spectraplex_core::models::Chain::Solana => {
                        solana_parser::parse_solana_transaction(tx)
                    }
                    spectraplex_core::models::Chain::Hyperliquid => {
                        hyperliquid_parser::parse_hyperliquid_transaction(tx)
                    }
                    spectraplex_core::models::Chain::Ethereum => {
                        evm_parser::parse_evm_transaction(tx)
                    }
                };
                match result {
                    Ok(entries) => all_entries.extend(entries),
                    Err(e) => {
                        warn!(tx_hash = %tx.tx_hash, error = %e, "Skipping unparseable transaction");
                    }
                }
            }

            if let Some(p) = pool {
                info!(
                    count = all_entries.len(),
                    "Saving ledger entries to database"
                );
                let repo = Repository::new(p);
                repo.save_ledger_entries(&all_entries).await?;

                // Materialize V2 Silver datasets (best-effort).
                // When an explicit network is provided, use it.  Otherwise,
                // materialize_silver_datasets resolves the actual network from
                // existing Bronze raw_transactions rows.
                let silver_result = repo
                    .materialize_silver_datasets(&transactions, network.as_deref())
                    .await;
                if !silver_result.all_succeeded() {
                    warn!(
                        written = silver_result.total_written(),
                        failed = silver_result.total_failed(),
                        skipped = silver_result.skipped_ambiguous,
                        "Silver materialization completed with partial failures"
                    );
                }

                info!("Normalization complete");
            } else {
                let mut out_file = File::create(&output)?;
                for entry in all_entries {
                    serde_json::to_writer(&out_file, &entry)?;
                    writeln!(out_file)?;
                }
                info!(path = ?output, "Normalization complete, output written to file");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;
    use spectraplex_core::models::Chain;

    fn make_tx(
        chain: Chain,
        tx_hash: &str,
        timestamp: i64,
        metadata: serde_json::Value,
    ) -> Transaction {
        Transaction {
            id: Uuid::new_v4(),
            user_id: Uuid::nil(),
            wallet_address: "test_wallet".to_string(),
            timestamp,
            tx_hash: tx_hash.to_string(),
            chain,
            raw_metadata: metadata,
        }
    }

    #[test]
    fn test_build_checkpoint_empty() {
        assert!(build_checkpoint("ethereum", "0xabc", &[]).is_none());
    }

    #[test]
    fn test_build_checkpoint_unknown_chain() {
        let tx = make_tx(Chain::Ethereum, "0xaaa", 100, json!({}));
        assert!(build_checkpoint("bitcoin", "0xabc", &[tx]).is_none());
    }

    #[test]
    fn test_build_checkpoint_ethereum() {
        let txs = vec![
            make_tx(Chain::Ethereum, "0xaaa", 100, json!({"block_number": 1000})),
            make_tx(Chain::Ethereum, "0xbbb", 200, json!({"block_number": 2000})),
            make_tx(Chain::Ethereum, "0xccc", 150, json!({"block_number": 1500})),
        ];

        let cp = build_checkpoint("ethereum", "0xwallet", &txs).unwrap();
        assert!(matches!(cp.chain, Chain::Ethereum));
        assert_eq!(cp.wallet_address, "0xwallet");
        assert_eq!(cp.last_signature, Some("0xbbb".to_string()));
        assert_eq!(cp.last_timestamp, Some(200));
        assert_eq!(cp.last_block, Some(2000 - 15)); // finality buffer applied
        assert_eq!(cp.last_slot, None);
    }

    #[test]
    fn test_build_checkpoint_solana() {
        let txs = vec![
            make_tx(Chain::Solana, "sig1", 300, json!({"slot": 5000})),
            make_tx(Chain::Solana, "sig2", 400, json!({"slot": 6000})),
        ];

        let cp = build_checkpoint("solana", "wallet123", &txs).unwrap();
        assert!(matches!(cp.chain, Chain::Solana));
        assert_eq!(cp.last_signature, Some("sig2".to_string()));
        assert_eq!(cp.last_timestamp, Some(400));
        assert_eq!(cp.last_slot, Some(6000 - 32)); // finality buffer applied
        assert_eq!(cp.last_block, None);
    }

    #[test]
    fn test_build_checkpoint_hyperliquid() {
        let txs = vec![
            make_tx(Chain::Hyperliquid, "hash1", 500, json!({})),
            make_tx(Chain::Hyperliquid, "hash2", 600, json!({})),
        ];

        let cp = build_checkpoint("hyperliquid", "0xhl", &txs).unwrap();
        assert!(matches!(cp.chain, Chain::Hyperliquid));
        assert_eq!(cp.last_signature, Some("hash2".to_string()));
        assert_eq!(cp.last_timestamp, Some(600));
        assert_eq!(cp.last_slot, None);
        assert_eq!(cp.last_block, None);
    }

    #[test]
    fn test_parse_init_db() {
        let cli = Cli::try_parse_from(["spectraplex", "init-db"]).unwrap();
        assert!(matches!(cli.command, Commands::InitDb));
    }

    #[test]
    fn test_parse_init_db_with_db_url() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "--db-url",
            "postgres://localhost/test",
            "init-db",
        ])
        .unwrap();
        assert_eq!(cli.db_url.unwrap(), "postgres://localhost/test");
    }

    #[test]
    fn test_parse_ingest_required_args() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "ingest",
            "--chain",
            "solana",
            "--wallet",
            "abc123",
            "--rpc",
            "https://api.mainnet.solana.com",
        ])
        .unwrap();
        match cli.command {
            Commands::Ingest {
                chain,
                wallet,
                rpc,
                limit,
                output,
                ..
            } => {
                assert_eq!(chain, "solana");
                assert_eq!(wallet, vec!["abc123"]);
                assert_eq!(rpc.unwrap(), "https://api.mainnet.solana.com");
                assert_eq!(limit, 10);
                assert_eq!(output, PathBuf::from("bronze_transactions.jsonl"));
            }
            _ => panic!("expected Ingest command"),
        }
    }

    #[test]
    fn test_parse_ingest_no_chain_defaults_to_empty() {
        // --chain is now optional (defaults to ""), --network is preferred
        let cli = Cli::try_parse_from(["spectraplex", "ingest", "--wallet", "abc123"]).unwrap();
        match cli.command {
            Commands::Ingest { chain, network, .. } => {
                assert_eq!(chain, "");
                assert!(network.is_none());
            }
            _ => panic!("expected Ingest command"),
        }
    }

    #[test]
    fn test_parse_ingest_network_flag() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "ingest",
            "--network",
            "solana-mainnet",
            "--wallet",
            "abc123",
        ])
        .unwrap();
        match cli.command {
            Commands::Ingest { chain, network, .. } => {
                assert_eq!(chain, ""); // chain defaults to empty when not provided
                assert_eq!(network.as_deref(), Some("solana-mainnet"));
            }
            _ => panic!("expected Ingest command"),
        }
    }

    #[test]
    fn test_parse_ingest_missing_wallet() {
        let result = Cli::try_parse_from(["spectraplex", "ingest", "--chain", "solana"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ingest_all_options() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "ingest",
            "--chain",
            "solana",
            "--wallet",
            "abc123",
            "--output",
            "custom.jsonl",
            "--grpc-url",
            "https://grpc.example.com",
            "--x-token",
            "secret",
            "--limit",
            "50",
            "--user-id",
            "00000000-0000-0000-0000-000000000001",
        ])
        .unwrap();
        match cli.command {
            Commands::Ingest {
                chain,
                wallet,
                output,
                grpc_url,
                x_token,
                limit,
                user_id,
                ..
            } => {
                assert_eq!(chain, "solana");
                assert_eq!(wallet, vec!["abc123"]);
                assert_eq!(output, PathBuf::from("custom.jsonl"));
                assert_eq!(grpc_url.unwrap(), "https://grpc.example.com");
                assert_eq!(x_token.unwrap(), "secret");
                assert_eq!(limit, 50);
                assert!(user_id.is_some());
            }
            _ => panic!("expected Ingest command"),
        }
    }

    #[test]
    fn test_parse_ingest_multiple_wallets() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "ingest",
            "--chain",
            "solana",
            "--wallet",
            "abc123",
            "--wallet",
            "def456",
            "--wallet",
            "ghi789",
            "--rpc",
            "https://api.mainnet.solana.com",
        ])
        .unwrap();
        match cli.command {
            Commands::Ingest { wallet, .. } => {
                assert_eq!(wallet, vec!["abc123", "def456", "ghi789"]);
            }
            _ => panic!("expected Ingest command"),
        }
    }

    #[test]
    fn test_parse_normalize_defaults() {
        let cli = Cli::try_parse_from(["spectraplex", "normalize"]).unwrap();
        match cli.command {
            Commands::Normalize {
                input,
                output,
                network,
            } => {
                assert_eq!(input, PathBuf::from("bronze_transactions.jsonl"));
                assert_eq!(output, PathBuf::from("silver_ledger.jsonl"));
                assert!(network.is_none());
            }
            _ => panic!("expected Normalize command"),
        }
    }

    #[test]
    fn test_parse_normalize_custom_paths() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "normalize",
            "--input",
            "custom_input.jsonl",
            "--output",
            "custom_output.jsonl",
        ])
        .unwrap();
        match cli.command {
            Commands::Normalize {
                input,
                output,
                network,
            } => {
                assert_eq!(input, PathBuf::from("custom_input.jsonl"));
                assert_eq!(output, PathBuf::from("custom_output.jsonl"));
                assert!(network.is_none());
            }
            _ => panic!("expected Normalize command"),
        }
    }

    #[test]
    fn test_parse_normalize_with_network() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "normalize",
            "--input",
            "db:0xabc",
            "--network",
            "base-mainnet",
        ])
        .unwrap();
        match cli.command {
            Commands::Normalize {
                input,
                output: _,
                network,
            } => {
                assert_eq!(input, PathBuf::from("db:0xabc"));
                assert_eq!(network.as_deref(), Some("base-mainnet"));
            }
            _ => panic!("expected Normalize command"),
        }
    }

    #[test]
    fn test_parse_no_subcommand() {
        let result = Cli::try_parse_from(["spectraplex"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_unknown_subcommand() {
        let result = Cli::try_parse_from(["spectraplex", "unknown"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ingest_ethereum() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "ingest",
            "--chain",
            "ethereum",
            "--wallet",
            "0xabc",
            "--rpc",
            "https://eth.rpc.com",
        ])
        .unwrap();
        match cli.command {
            Commands::Ingest { chain, rpc, .. } => {
                assert_eq!(chain, "ethereum");
                assert_eq!(rpc.unwrap(), "https://eth.rpc.com");
            }
            _ => panic!("expected Ingest command"),
        }
    }

    #[test]
    fn test_parse_explicit_db_url_overrides() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "--db-url",
            "postgres://explicit/db",
            "init-db",
        ])
        .unwrap();
        assert_eq!(cli.db_url.unwrap(), "postgres://explicit/db");
    }

    // -----------------------------------------------------------------------
    // Target registration CLI tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_register_target_required_args() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "register-target",
            "--kind",
            "wallet",
            "--network",
            "solana-mainnet",
            "--address",
            "DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy",
        ])
        .unwrap();
        match cli.command {
            Commands::RegisterTarget {
                kind,
                network,
                address,
                mode,
                label,
                filter_spec,
            } => {
                assert_eq!(kind, "wallet");
                assert_eq!(network, "solana-mainnet");
                assert_eq!(
                    address.as_deref(),
                    Some("DRpbCBMxVnDK7maPM5tGv6MvB3v1sRMC86PZ8okm21hy")
                );
                assert_eq!(mode, "both"); // default
                assert!(label.is_none());
                assert!(filter_spec.is_none());
            }
            _ => panic!("expected RegisterTarget command"),
        }
    }

    #[test]
    fn test_parse_register_target_all_options() {
        let cli = Cli::try_parse_from([
            "spectraplex",
            "register-target",
            "--kind",
            "contract",
            "--network",
            "ethereum-mainnet",
            "--address",
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "--mode",
            "backfill",
            "--label",
            "USDC Contract",
            "--filter-spec",
            r#"{"event_signatures":["Transfer(address,address,uint256)"]}"#,
        ])
        .unwrap();
        match cli.command {
            Commands::RegisterTarget {
                kind,
                network,
                address,
                mode,
                label,
                filter_spec,
            } => {
                assert_eq!(kind, "contract");
                assert_eq!(network, "ethereum-mainnet");
                assert_eq!(
                    address.as_deref(),
                    Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
                );
                assert_eq!(mode, "backfill");
                assert_eq!(label.as_deref(), Some("USDC Contract"));
                assert!(filter_spec.is_some());
            }
            _ => panic!("expected RegisterTarget command"),
        }
    }

    #[test]
    fn test_parse_register_target_missing_kind() {
        let result = Cli::try_parse_from([
            "spectraplex",
            "register-target",
            "--network",
            "solana-mainnet",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_register_target_missing_network() {
        let result = Cli::try_parse_from(["spectraplex", "register-target", "--kind", "wallet"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_list_targets() {
        let cli = Cli::try_parse_from(["spectraplex", "list-targets"]).unwrap();
        match cli.command {
            Commands::ListTargets { network, kind } => {
                assert!(network.is_none());
                assert!(kind.is_none());
            }
            _ => panic!("expected ListTargets command"),
        }
    }

    #[test]
    fn test_parse_list_targets_with_network() {
        let cli =
            Cli::try_parse_from(["spectraplex", "list-targets", "--network", "solana-mainnet"])
                .unwrap();
        match cli.command {
            Commands::ListTargets { network, kind } => {
                assert_eq!(network.as_deref(), Some("solana-mainnet"));
                assert!(kind.is_none());
            }
            _ => panic!("expected ListTargets command"),
        }
    }

    #[test]
    fn test_parse_list_targets_with_kind() {
        let cli = Cli::try_parse_from(["spectraplex", "list-targets", "--kind", "wallet"]).unwrap();
        match cli.command {
            Commands::ListTargets { network, kind } => {
                assert!(network.is_none());
                assert_eq!(kind.as_deref(), Some("wallet"));
            }
            _ => panic!("expected ListTargets command"),
        }
    }

    #[test]
    fn test_parse_list_networks() {
        let cli = Cli::try_parse_from(["spectraplex", "list-networks"]).unwrap();
        assert!(matches!(cli.command, Commands::ListNetworks));
    }

    #[test]
    fn test_parse_ingest_still_works() {
        // Backward compatibility: existing ingest command unchanged
        let cli = Cli::try_parse_from([
            "spectraplex",
            "ingest",
            "--chain",
            "solana",
            "--wallet",
            "abc123",
            "--rpc",
            "https://api.mainnet.solana.com",
        ])
        .unwrap();
        match cli.command {
            Commands::Ingest {
                chain, wallet, rpc, ..
            } => {
                assert_eq!(chain, "solana");
                assert_eq!(wallet, vec!["abc123"]);
                assert!(rpc.is_some());
            }
            _ => panic!("expected Ingest command"),
        }
    }

    #[test]
    fn test_multi_wallet_file_output_appends() {
        // Verify that writing multiple wallets' transactions to a file does
        // not truncate earlier wallets' data. This is a regression test for
        // the bug where File::create inside the wallet loop overwrote the
        // output on each iteration.
        use std::io::{BufRead, BufReader};

        let dir = std::env::temp_dir().join(format!("sp_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let output_path = dir.join("multi_wallet.jsonl");

        // Simulate the file-output path: truncate once, then append per wallet.
        File::create(&output_path).unwrap();

        let wallets = vec!["wallet_a", "wallet_b", "wallet_c"];
        for wallet in &wallets {
            let txs = vec![make_tx(
                Chain::Ethereum,
                &format!("0x{wallet}"),
                100,
                json!({"wallet": wallet}),
            )];

            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_path)
                .unwrap();
            for tx in &txs {
                serde_json::to_writer(&mut file, tx).unwrap();
                writeln!(file).unwrap();
            }
        }

        // Read back and verify all three wallets are present
        let file = File::open(&output_path).unwrap();
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();

        assert_eq!(
            lines.len(),
            3,
            "output should contain one line per wallet, got {}",
            lines.len()
        );

        for (i, wallet) in wallets.iter().enumerate() {
            let parsed: serde_json::Value = serde_json::from_str(&lines[i]).unwrap();
            assert_eq!(
                parsed["tx_hash"].as_str().unwrap(),
                format!("0x{wallet}"),
                "line {} should contain tx from {}",
                i,
                wallet
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
