# Spectraplex

[![CI](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml)
[![Security Audit](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml)

Spectraplex is a multi-chain blockchain transaction indexer written in Rust. It ingests raw transactions from supported blockchains, normalizes them into structured ledger entries, and serves the data through both a CLI and REST API. Built around a **Bronze/Silver data layer architecture**, it transforms messy on-chain data into clean, queryable financial records.

## Supported Chains

| Chain | Ingestion | Parsing | Real-time Streaming | Status |
|-------|-----------|---------|---------------------|--------|
| Solana | RPC + gRPC | SOL + SPL tokens | Yellowstone gRPC | Active |
| Hyperliquid | REST + WebSocket | Planned | WebSocket | In progress |
| Ethereum (EVM) | alloy | Planned | Planned | In progress |

## Architecture

```
                    CLI / REST API (Axum @ 127.0.0.1:3000)
                               |
                +--------------+--------------+
                v              v              v
            Ingest         Normalize        Query
                |              |              |
                v              v              v
          +--------------------------------------+
          |      Repository Layer (SQLx)         |
          +----------------+---------------------+
                           v
                 +-------------------+
                 |    PostgreSQL     |
                 |  - transactions   |  <-- Bronze (raw JSONB)
                 |  - ledger_entries |  <-- Silver (normalized)
                 +-------------------+

          +----------------------------------+
          |  Blockchain Adapters             |
          |  - SolanaAdapter (RPC)           |
          |  - SolanaGrpcAdapter (gRPC)      |
          |  - HyperliquidAdapter (planned)  |
          |  - EvmAdapter (planned)          |
          +----------------------------------+
                         |
                         v
                   Chain Parsers
             (raw tx -> ledger entries)
```

### Data Flow

1. **Ingest (Bronze)** -- Fetch raw transactions from a blockchain RPC/gRPC endpoint. Store the full transaction as JSONB in the `transactions` table.
2. **Normalize (Silver)** -- Read raw transactions, parse balance changes (native tokens, SPL tokens, etc.), and write structured `ledger_entries` with fields like asset, amount, and entry type (trade, fee, transfer, staking, income).
3. **Query** -- Retrieve transactions or ledger entries by wallet address via the API or CLI.

## Quick Start

### Prerequisites

- **Rust** (stable, 2021 edition) -- [install via rustup](https://rustup.rs/)
- **PostgreSQL** (14+) -- running locally or remotely
- A Solana RPC endpoint (e.g., `https://api.mainnet-beta.solana.com`)

### 1. Clone and build

```bash
git clone https://github.com/user1303836/spectraplex.git
cd spectraplex
cargo build --release
```

### 2. Configure the database

Create a PostgreSQL database and set the connection URL:

```bash
createdb spectraplex
echo "DATABASE_URL=postgresql://localhost/spectraplex" > .env
```

### 3. Run migrations

```bash
cargo run --release --bin spectraplex-cli -- init-db --db-url postgresql://localhost/spectraplex
```

### 4. Ingest transactions

Fetch Solana transactions for a wallet via RPC:

```bash
cargo run --release --bin spectraplex-cli -- ingest \
  --chain solana \
  --wallet <WALLET_ADDRESS> \
  --rpc https://api.mainnet-beta.solana.com \
  --limit 10
```

Or via Yellowstone gRPC (if you have an endpoint):

```bash
cargo run --release --bin spectraplex-cli -- ingest \
  --chain solana \
  --wallet <WALLET_ADDRESS> \
  --grpc-url https://your-grpc-endpoint.com \
  --x-token <AUTH_TOKEN> \
  --limit 10
```

### 5. Normalize to ledger entries

From the database:

```bash
cargo run --release --bin spectraplex-cli -- normalize --input db:<WALLET_ADDRESS>
```

Or from a JSONL file (if you ran ingest without `--db-url`):

```bash
cargo run --release --bin spectraplex-cli -- normalize \
  --input bronze_transactions.jsonl \
  --output silver_ledger.jsonl
```

### 6. Start the API server

```bash
cargo run --release --bin spectraplex-api
# Listening on 127.0.0.1:3000
```

### 7. Query the data

```bash
# Health check
curl http://127.0.0.1:3000/health

# Get raw transactions for a wallet
curl http://127.0.0.1:3000/v1/transactions/<WALLET_ADDRESS>

# Get normalized ledger entries
curl http://127.0.0.1:3000/v1/ledger/<WALLET_ADDRESS>
```

## CLI Reference

The CLI binary is `spectraplex-cli`. All commands accept a global `--db-url` flag (or `DATABASE_URL` env var).

| Command | Description |
|---------|-------------|
| `init-db` | Run PostgreSQL migrations to create/update the schema |
| `ingest` | Fetch raw transactions from a blockchain and store them |
| `normalize` | Parse raw transactions into structured ledger entries |

### `ingest` flags

| Flag | Required | Default | Description |
|------|----------|---------|-------------|
| `-c, --chain` | Yes | -- | Blockchain name (`solana`) |
| `-w, --wallet` | Yes | -- | Wallet address to index |
| `-o, --output` | No | `bronze_transactions.jsonl` | Output file (when no DB) |
| `--rpc` | No* | -- | Solana RPC URL |
| `--grpc-url` | No* | -- | Yellowstone gRPC endpoint |
| `--x-token` | No | -- | gRPC auth token |
| `--limit` | No | `10` | Max transactions to fetch |

*Either `--rpc` or `--grpc-url` is required for Solana.

### `normalize` flags

| Flag | Required | Default | Description |
|------|----------|---------|-------------|
| `-i, --input` | No | `bronze_transactions.jsonl` | Input file or `db:<wallet>` |
| `-o, --output` | No | `silver_ledger.jsonl` | Output file (when no DB) |

## API Reference

The API server runs on `127.0.0.1:3000` and requires `DATABASE_URL` to be set.

| Method | Endpoint | Description |
|--------|----------|-------------|
| `GET` | `/health` | Health check, returns `"OK"` |
| `POST` | `/v1/ingest` | Trigger ingestion for a wallet |
| `POST` | `/v1/normalize` | Normalize transactions for a wallet |
| `GET` | `/v1/transactions/:wallet` | Get raw transactions by wallet |
| `GET` | `/v1/ledger/:wallet` | Get normalized ledger entries by wallet |

### POST /v1/ingest

```bash
curl -X POST http://127.0.0.1:3000/v1/ingest \
  -H "Content-Type: application/json" \
  -d '{"_chain": "solana", "wallet": "<WALLET>", "rpc_url": "https://api.mainnet-beta.solana.com"}'
```

### POST /v1/normalize

```bash
curl -X POST http://127.0.0.1:3000/v1/normalize \
  -H "Content-Type: application/json" \
  -d '{"wallet": "<WALLET>"}'
```

## Configuration

Spectraplex reads configuration from environment variables. You can use a `.env` file in the project root.

| Variable | Required | Description |
|----------|----------|-------------|
| `DATABASE_URL` | Yes (for DB mode) | PostgreSQL connection string |

The CLI also supports `--db-url` as a command-line flag. When neither is provided, the CLI falls back to file-based JSONL storage.

## Project Structure

```
spectraplex/
+-- core/               Shared data models (Transaction, LedgerEntry, Chain, EntryType)
|                       and the ChainIngestor trait
+-- adapters/           Blockchain adapters (Solana RPC, gRPC), transaction parsers,
|                       and the PostgreSQL repository layer
+-- cli/                CLI binary: init-db, ingest, normalize commands
+-- api/                REST API binary (Axum): ingest, normalize, query endpoints
+-- migrations/         PostgreSQL schema migrations (SQLx)
```

### Data Models (`core/src/models.rs`)

- **`Transaction`** (Bronze) -- Raw blockchain transaction with JSONB metadata
- **`LedgerEntry`** (Silver) -- Normalized entry: asset symbol, amount, type (trade/fee/transfer/staking/income)
- **`Chain`** -- Enum: Solana, Hyperliquid, Ethereum
- **`ChainIngestor`** -- Async trait that all chain adapters implement

## Development

### Run tests

```bash
cargo test --workspace
```

### Run lints

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Adding a new chain adapter

1. Add a new module in `adapters/src/` (e.g., `my_chain.rs`)
2. Implement the `ChainIngestor` trait from `spectraplex-core`
3. Add a parser module that converts raw transactions to `LedgerEntry` records
4. Wire it into the CLI (`cli/src/main.rs`) and API (`api/src/main.rs`)
5. Add the chain variant to the `Chain` enum in `core/src/models.rs`

### Database schema

The schema uses two main tables:

- **`transactions`** -- Bronze layer. Stores raw blockchain data as JSONB. Indexed by wallet address and timestamp.
- **`ledger_entries`** -- Silver layer. Normalized financial records. Indexed by wallet address and creation time.

Both tables use UUIDs as primary keys and support idempotent inserts (`ON CONFLICT DO NOTHING`).

## Tech Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust (Edition 2021) |
| Async Runtime | Tokio |
| Database | PostgreSQL + SQLx |
| API Framework | Axum 0.7 |
| CLI Framework | Clap 4 (derive) |
| Solana | solana-sdk 3.0, Yellowstone gRPC |
| Serialization | serde / serde_json |
| Precision Math | BigDecimal |

## Roadmap

- [x] Solana RPC ingestion and transaction parsing
- [x] Bronze/Silver data layer with PostgreSQL
- [x] CLI and REST API
- [x] CI/CD with GitHub Actions
- [ ] Yellowstone gRPC real-time streaming
- [ ] Hyperliquid adapter (REST + WebSocket)
- [ ] EVM adapter (Ethereum and compatibles)
- [ ] Improved entry type classification (trades, fees, staking)
- [ ] Historical price lookups for fiat value
- [ ] Docker Compose deployment
- [ ] Incremental sync with checkpointing
- [ ] User identity and wallet management
- [ ] Cross-chain portfolio views

## License

See [LICENSE](LICENSE) for details.
