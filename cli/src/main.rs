use clap::{Parser, Subcommand};
use adapters::solana::SolanaAdapter;
use core::models::ChainIngestor;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Ingests blockchain transactions for tax calculations", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Parser)]
enum Commands {
    Ingest {
        #[arg(short, long)]
        chain: String,

        #[arg(short, long)]
        wallet: String,

        #[arg(short, long, default_value = "output.csv")]
        output: PathBuf,
        
        #[arg(long)]
        rpc: Option<String>,

        #[arg(long)]
        grpc_url: Option<String>,

        #[arg(long)]
        x_token: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Ingest { chain, wallet, output, rpc } => {
            println!("Starting ingestion for {} on chain {}", wallet, chain);

            let events = match chain.as_str() {
                "solana" => {
                    let endpoint = grpc_url.expect("gRPC URL required for Solana");
                    let adapter = SolanaGrpcAdapter::new(&endpoint, x_token);
                    adapter.fetch_history(&wallet, 50).await?
                }
                "hyperliquid" => {
                    println!("Hyperliquid adapter not yet implemented!");
                    return Ok(());
                }
                _ => {
                    println!("Unsupported chain: {}", chain);
                    return Ok(());
                }
            };

            // Write to CSV
            let mut wtr = csv::Writer::from_path(output)?;
            for event in events {
                wtr.serialize(event)?;
            }
            println!("Done! Data written to CSV.");
        }
    }

    Ok(())
}