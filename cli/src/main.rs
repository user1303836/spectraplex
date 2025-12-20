use clap::{Parser, Subcommand};
use spectraplex_adapters::{solana::SolanaAdapter, solana_grpc::SolanaGrpcAdapter, solana_parser, repo::Repository};
use spectraplex_core::models::{ChainIngestor, Transaction};
use std::path::PathBuf;
use std::fs::File;
use std::io::{Write, BufReader, BufRead};
use sqlx::postgres::PgPoolOptions;

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
    },
    /// Normalize Bronze data to Silver layer (Ledger Entries)
    Normalize {
        #[arg(short, long, default_value = "bronze_transactions.jsonl")]
        input: PathBuf,

        #[arg(short, long, default_value = "silver_ledger.jsonl")]
        output: PathBuf,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
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
                println!("Running migrations...");
                sqlx::migrate!("../migrations").run(&p).await?;
                println!("Database initialized successfully.");
            } else {
                println!("Error: --db-url is required for InitDb");
            }
        }
        Commands::Ingest { chain, wallet, output, rpc, grpc_url, x_token, limit } => {
            println!("Starting ingestion for {} on chain {}", wallet, chain);

            let events = match chain.as_str() {
                "solana" => {
                    if let Some(endpoint) = grpc_url {
                        let adapter = SolanaGrpcAdapter::new(&endpoint, x_token);
                        adapter.fetch_history(&wallet, limit).await?
                    } else if let Some(rpc_url) = rpc {
                        let adapter = SolanaAdapter::new(&rpc_url);
                        adapter.fetch_history(&wallet, limit).await?
                    } else {
                        anyhow::bail!("Either --grpc-url or --rpc must be provided for Solana");
                    }
                }
                _ => {
                    println!("Unsupported chain: {}", chain);
                    return Ok(());
                }
            };

            // Strategy: DB first, fallback to File
            if let Some(p) = pool {
                let repo = Repository::new(p);
                repo.save_transactions(&events).await?;
                println!("Saved {} transactions to Database.", events.len());
            } else {
                // Write to JSONL
                let mut file = File::create(&output)?;
                for event in events {
                    serde_json::to_writer(&file, &event)?;
                    writeln!(file)?;
                }
                println!("Done! Data written to {:?}", output);
            }
        }
        Commands::Normalize { input, output } => {
            let transactions = if let Some(p) = pool.clone() {
                
                let input_str = input.to_string_lossy();
                if input_str.starts_with("db:") {
                    let wallet = input_str.strip_prefix("db:").unwrap();
                    println!("Fetching transactions for wallet {} from DB...", wallet);
                    let repo = Repository::new(p);
                    repo.get_transactions_by_wallet(wallet).await?
                } else {
                    println!("Reading raw data from {:?}...", input);
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
                println!("Reading raw data from {:?}...", input);
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
                    },
                    _ => {
                        println!("Skipping unsupported chain for normalization: {:?}", tx.chain);
                        vec![]
                    }
                };
                all_entries.extend(entries);
            }

            if let Some(p) = pool {
                println!("Saving {} ledger entries to Database...", all_entries.len());
                let repo = Repository::new(p);
                repo.save_ledger_entries(&all_entries).await?;
                println!("Done.");
            } else {
                let mut out_file = File::create(&output)?;
                for entry in all_entries {
                    serde_json::to_writer(&out_file, &entry)?;
                    writeln!(out_file)?;
                }
                println!("Normalization complete. Output written to {:?}", output);
            }
        }
    }

    Ok(())
}
