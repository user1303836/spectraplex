use clap::{Parser, Subcommand};
use spectraplex_adapters::{
    evm::EvmAdapter, evm_parser, hyperliquid::HyperliquidAdapter, hyperliquid_parser,
    repo::Repository, solana::SolanaAdapter, solana_grpc::SolanaGrpcAdapter, solana_parser,
};
use spectraplex_core::models::{Chain, ChainIngestor, IndexerCheckpoint, Transaction};
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
        #[arg(short, long)]
        chain: String,

        #[arg(short, long)]
        wallet: String,

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
    },
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
            if let Some(p) = pool {
                info!("Running migrations...");
                sqlx::migrate!("../migrations").run(&p).await?;
                info!("Database initialized successfully.");
            } else {
                error!("--db-url is required for InitDb");
            }
        }
        Commands::Ingest {
            chain,
            wallet,
            output,
            rpc,
            grpc_url,
            x_token,
            limit,
            user_id,
        } => {
            let user_id = user_id.unwrap_or_else(|| {
                let id = Uuid::new_v4();
                info!(user_id = %id, "No --user-id provided, auto-generated");
                id
            });
            info!(wallet = %wallet, chain = %chain, "Starting ingestion");

            let checkpoint = if let Some(ref p) = pool {
                let repo = Repository::new(p.clone());
                match repo.get_checkpoint(&chain, &wallet).await? {
                    Some(cp) => {
                        info!(
                            chain = %chain,
                            wallet = %wallet,
                            last_signature = ?cp.last_signature,
                            last_slot = ?cp.last_slot,
                            last_timestamp = ?cp.last_timestamp,
                            "Resuming from checkpoint"
                        );
                        Some(cp)
                    }
                    None => {
                        info!(chain = %chain, wallet = %wallet, "No existing checkpoint, full ingestion");
                        None
                    }
                }
            } else {
                None
            };

            let events = match chain.as_str() {
                "solana" => {
                    if let Some(endpoint) = grpc_url {
                        let adapter = SolanaGrpcAdapter::new(&endpoint, x_token);
                        if let Some(ref cp) = checkpoint {
                            if let Some(slot) = cp.last_slot {
                                adapter.checkpoint().update(slot as u64);
                            }
                        }
                        adapter.fetch_history(&wallet, limit, user_id).await?
                    } else if let Some(rpc_url) = rpc {
                        let adapter = SolanaAdapter::new(&rpc_url);
                        adapter.fetch_history(&wallet, limit, user_id).await?
                    } else {
                        anyhow::bail!("Either --grpc-url or --rpc must be provided for Solana");
                    }
                }
                "hyperliquid" => {
                    let adapter = HyperliquidAdapter::new();
                    adapter.fetch_history(&wallet, limit, user_id).await?
                }
                "ethereum" => {
                    let rpc_url =
                        rpc.ok_or_else(|| anyhow::anyhow!("--rpc is required for Ethereum"))?;
                    let adapter = EvmAdapter::new(&rpc_url).await?;
                    adapter.fetch_history(&wallet, limit, user_id).await?
                }
                _ => {
                    warn!(chain = %chain, "Unsupported chain");
                    return Ok(());
                }
            };

            // Strategy: DB first, fallback to File
            if let Some(p) = pool {
                let repo = Repository::new(p);
                repo.save_transactions(&events).await?;
                info!(count = events.len(), "Saved transactions to database");

                if let Some(cp) = build_checkpoint(&chain, &wallet, &events) {
                    repo.save_checkpoint(&cp).await?;
                    info!(
                        chain = %chain,
                        wallet = %wallet,
                        last_signature = ?cp.last_signature,
                        last_slot = ?cp.last_slot,
                        last_timestamp = ?cp.last_timestamp,
                        "Checkpoint saved"
                    );
                }
            } else {
                // Write to JSONL
                let mut file = File::create(&output)?;
                for event in events {
                    serde_json::to_writer(&file, &event)?;
                    writeln!(file)?;
                }
                info!(path = ?output, "Data written to file");
            }
        }
        Commands::Normalize { input, output } => {
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

            for tx in transactions {
                let result = match tx.chain {
                    spectraplex_core::models::Chain::Solana => {
                        solana_parser::parse_solana_transaction(&tx)
                    }
                    spectraplex_core::models::Chain::Hyperliquid => {
                        hyperliquid_parser::parse_hyperliquid_transaction(&tx)
                    }
                    spectraplex_core::models::Chain::Ethereum => {
                        evm_parser::parse_evm_transaction(&tx)
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

fn build_checkpoint(chain: &str, wallet: &str, txs: &[Transaction]) -> Option<IndexerCheckpoint> {
    if txs.is_empty() {
        return None;
    }

    let chain_enum = match chain {
        "solana" => Chain::Solana,
        "ethereum" => Chain::Ethereum,
        "hyperliquid" => Chain::Hyperliquid,
        _ => return None,
    };

    let latest = txs.iter().max_by_key(|tx| tx.timestamp)?;

    let last_signature = Some(latest.tx_hash.clone());
    let last_timestamp = Some(latest.timestamp);

    let last_slot = match chain {
        "ethereum" => txs
            .iter()
            .filter_map(|tx| tx.raw_metadata.get("block_number").and_then(|v| v.as_i64()))
            .max(),
        "solana" => txs
            .iter()
            .filter_map(|tx| tx.raw_metadata.get("slot").and_then(|v| v.as_i64()))
            .max(),
        _ => None,
    };

    Some(IndexerCheckpoint {
        chain: chain_enum,
        wallet_address: wallet.to_string(),
        last_signature,
        last_slot,
        last_timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

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
        assert_eq!(cp.last_slot, Some(2000));
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
        assert_eq!(cp.last_slot, Some(6000));
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
                assert_eq!(wallet, "abc123");
                assert_eq!(rpc.unwrap(), "https://api.mainnet.solana.com");
                assert_eq!(limit, 10);
                assert_eq!(output, PathBuf::from("bronze_transactions.jsonl"));
            }
            _ => panic!("expected Ingest command"),
        }
    }

    #[test]
    fn test_parse_ingest_missing_chain() {
        let result = Cli::try_parse_from(["spectraplex", "ingest", "--wallet", "abc123"]);
        assert!(result.is_err());
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
                assert_eq!(wallet, "abc123");
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
    fn test_parse_normalize_defaults() {
        let cli = Cli::try_parse_from(["spectraplex", "normalize"]).unwrap();
        match cli.command {
            Commands::Normalize { input, output } => {
                assert_eq!(input, PathBuf::from("bronze_transactions.jsonl"));
                assert_eq!(output, PathBuf::from("silver_ledger.jsonl"));
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
            Commands::Normalize { input, output } => {
                assert_eq!(input, PathBuf::from("custom_input.jsonl"));
                assert_eq!(output, PathBuf::from("custom_output.jsonl"));
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
}
