use clap::{Parser, Subcommand};
use spectraplex_adapters::{
    evm::EvmAdapter, evm_parser, hyperliquid::HyperliquidAdapter, hyperliquid_parser,
    repo::Repository, solana::SolanaAdapter, solana_grpc::SolanaGrpcAdapter, solana_parser,
};
use spectraplex_core::models::{ChainIngestor, Transaction};
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
    dotenv::dotenv().ok();

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

            let events = match chain.as_str() {
                "solana" => {
                    if let Some(endpoint) = grpc_url {
                        let adapter = SolanaGrpcAdapter::new(&endpoint, x_token);
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
            let transactions = if let Some(p) = pool.clone() {
                let input_str = input.to_string_lossy();
                if input_str.starts_with("db:") {
                    let wallet = input_str.strip_prefix("db:").unwrap();
                    info!(wallet = %wallet, "Fetching transactions from DB");
                    let repo = Repository::new(p);
                    repo.get_transactions_by_wallet(wallet).await?
                } else {
                    info!(path = ?input, "Reading raw data from file");
                    let file = File::open(&input)?;
                    let reader = BufReader::new(file);
                    let mut txs = Vec::new();
                    for line in reader.lines() {
                        let line = line?;
                        let tx: Transaction = serde_json::from_str(&line)?;
                        txs.push(tx);
                    }
                    txs
                }
            } else {
                info!(path = ?input, "Reading raw data from file");
                let file = File::open(&input)?;
                let reader = BufReader::new(file);
                let mut txs = Vec::new();
                for line in reader.lines() {
                    let line = line?;
                    let tx: Transaction = serde_json::from_str(&line)?;
                    txs.push(tx);
                }
                txs
            };

            let mut all_entries = Vec::new();

            for tx in transactions {
                // Use the parser to extract actual ledger entries
                let entries = match tx.chain {
                    spectraplex_core::models::Chain::Solana => {
                        solana_parser::parse_solana_transaction(&tx)?
                    }
                    spectraplex_core::models::Chain::Hyperliquid => {
                        hyperliquid_parser::parse_hyperliquid_transaction(&tx)?
                    }
                    spectraplex_core::models::Chain::Ethereum => {
                        evm_parser::parse_evm_transaction(&tx)?
                    }
                    _ => {
                        warn!(chain = ?tx.chain, "Skipping unsupported chain for normalization");
                        vec![]
                    }
                };
                all_entries.extend(entries);
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
