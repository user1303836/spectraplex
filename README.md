# Spectraplex

[![CI](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml)
[![Security Audit](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml)

Spectraplex is a multi-chain blockchain transaction indexer written in Rust. It ingests raw transactions from supported blockchains, normalizes them into structured ledger entries, and serves the data through both a CLI and REST API. Built around a **Bronze/Silver data layer architecture**, it transforms messy on-chain data into clean, queryable financial records.

## Supported Chains

| Chain | Ingestion | Parsing | Real-time Streaming | Status |
|-------|-----------|---------|---------------------|--------|
| Solana | RPC + gRPC | SOL + SPL tokens (symbol lookup) | Yellowstone gRPC | Active |
| Hyperliquid | REST + WebSocket | Fills, deposits, withdrawals | WebSocket | Active |
| Ethereum (EVM) | alloy (eth_getLogs) | ERC-20 transfers (uint256 precision) | Planned | Active |

## Architecture

```
                    CLI / REST API (Axum, configurable host/port)
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
          |  - HyperliquidAdapter (REST+WS)  |
          |  - EvmAdapter (alloy)            |
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

Fetch Ethereum (or any EVM chain) transactions for a wallet:

```bash
cargo run --release --bin spectraplex-cli -- ingest \
  --chain ethereum \
  --wallet <ETH_ADDRESS> \
  --rpc https://eth-mainnet.g.alchemy.com/v2/<API_KEY> \
  --limit 5
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
# Listening on 127.0.0.1:3000 (configurable via SPECTRAPLEX_HOST / SPECTRAPLEX_PORT)
```

### 7. Query the data

```bash
# Health check
curl http://127.0.0.1:3000/health

# Get raw transactions for a wallet (paginated)
curl "http://127.0.0.1:3000/v1/transactions/<WALLET_ADDRESS>?limit=50&offset=0"

# Get normalized ledger entries (paginated)
curl "http://127.0.0.1:3000/v1/ledger/<WALLET_ADDRESS>?limit=50&offset=0"
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
| `-c, --chain` | Yes | -- | Blockchain name (`solana`, `ethereum`) |
| `-w, --wallet` | Yes | -- | Wallet address to index |
| `-o, --output` | No | `bronze_transactions.jsonl` | Output file (when no DB) |
| `--rpc` | No* | -- | RPC URL (Solana RPC or EVM JSON-RPC) |
| `--grpc-url` | No* | -- | Yellowstone gRPC endpoint |
| `--x-token` | No | -- | gRPC auth token |
| `--limit` | No | `10` | Max transactions to fetch |

*`--rpc` is required for Solana (or `--grpc-url`) and Ethereum.

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
| `POST` | `/v1/ingest` | Trigger async ingestion for a wallet (returns job ID) |
| `POST` | `/v1/normalize` | Trigger async normalization for a wallet (returns job ID) |
| `GET` | `/v1/jobs/:job_id` | Poll job status (pending/running/completed/failed) |
| `GET` | `/v1/transactions/:wallet` | Get raw transactions by wallet (paginated) |
| `GET` | `/v1/ledger/:wallet` | Get normalized ledger entries by wallet (paginated) |

All wallet endpoints validate the address format and return structured JSON errors.

Query endpoints support `?limit=N&offset=N` parameters (default limit: 50, max: 1000).

### POST /v1/ingest

```bash
curl -X POST http://127.0.0.1:3000/v1/ingest \
  -H "Content-Type: application/json" \
  -d '{"chain": "solana", "wallet": "<WALLET>"}'

# Response: {"id": "<JOB_UUID>", "state": "pending", "message": "Job queued"}
```

### POST /v1/normalize

```bash
curl -X POST http://127.0.0.1:3000/v1/normalize \
  -H "Content-Type: application/json" \
  -d '{"wallet": "<WALLET>"}'

# Response: {"id": "<JOB_UUID>", "state": "pending", "message": "Job queued"}
```

### GET /v1/jobs/:job_id

```bash
curl http://127.0.0.1:3000/v1/jobs/<JOB_UUID>

# Response: {"id": "<JOB_UUID>", "state": "completed", "message": "Ingested 42 transactions"}
```

Jobs are kept in memory for 1 hour after completion, then automatically pruned.

## Configuration

Spectraplex uses a layered configuration system powered by [figment](https://crates.io/crates/figment). Settings are loaded in order of priority:

1. Built-in defaults
2. `spectraplex.toml` (optional config file in project root)
3. `SPECTRAPLEX_*` environment variables (e.g., `SPECTRAPLEX_PORT=8080`)
4. Direct env vars: `DATABASE_URL`, `SOLANA_RPC_URL`, `EVM_RPC_URL`

| Variable | Default | Description |
|----------|---------|-------------|
| `DATABASE_URL` | *(required)* | PostgreSQL connection string |
| `SPECTRAPLEX_HOST` | `127.0.0.1` | API server bind address |
| `SPECTRAPLEX_PORT` | `3000` | API server port |
| `SPECTRAPLEX_POOL_SIZE` | `10` | Database connection pool size |
| `SPECTRAPLEX_LOG_LEVEL` | `info` | Log level (trace/debug/info/warn/error) |
| `SPECTRAPLEX_INGEST_LIMIT` | `50` | Default transaction fetch limit for API ingestion |
| `SOLANA_RPC_URL` | `https://api.mainnet-beta.solana.com` | Solana RPC endpoint |
| `EVM_RPC_URL` | `https://eth.llamarpc.com` | EVM JSON-RPC endpoint |

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

- **`Transaction`** (Bronze) -- Raw blockchain transaction with JSONB metadata, per-user scoping via `user_id`
- **`LedgerEntry`** (Silver) -- Normalized entry: asset symbol, amount, type (trade/fee/transfer/staking/income), with fiat value support
- **`Chain`** -- Enum: Solana, Hyperliquid, Ethereum
- **`ChainIngestor`** -- Async trait that all chain adapters implement (`fetch_history`)

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

The schema uses the following tables:

- **`transactions`** -- Bronze layer. Stores raw blockchain data as JSONB. Indexed by wallet address and timestamp.
- **`ledger_entries`** -- Silver layer. Normalized financial records. Indexed by wallet address and creation time.
- **`blocks`** -- Stores block hashes per chain for reorg detection (EVM chains).
- **`evm_logs`** -- Raw EVM event logs linked to transactions.

- **`indexer_checkpoints`** -- Tracks last-processed block per chain/wallet for incremental syncing.

All tables use UUIDs as primary keys and support idempotent batch inserts (`ON CONFLICT DO NOTHING`, 500 rows/batch).

## Tech Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust (Edition 2021) |
| Async Runtime | Tokio |
| Database | PostgreSQL + SQLx |
| API Framework | Axum 0.7 |
| CLI Framework | Clap 4 (derive) |
| Solana | solana-sdk 3.0, Yellowstone gRPC |
| Ethereum (EVM) | alloy 1.x, governor (rate limiting) |
| Serialization | serde / serde_json |
| Precision Math | BigDecimal |

## Roadmap

- [x] Solana RPC ingestion and transaction parsing
- [x] SPL token symbol resolution (USDC, USDT, BONK, JUP, etc.)
- [x] Bronze/Silver data layer with PostgreSQL
- [x] Batch SQL inserts (500 rows/batch)
- [x] CLI and REST API with async job system
- [x] Structured JSON error responses
- [x] Pagination on all query endpoints
- [x] Input validation on wallet addresses
- [x] CI/CD with GitHub Actions (fmt, clippy, test, security audit)
- [x] Yellowstone gRPC real-time streaming adapter
- [x] Hyperliquid adapter (REST + WebSocket + parser)
- [x] EVM adapter (Ethereum and compatibles) with uint256 precision
- [x] Layered configuration (defaults / TOML / env vars)
- [x] Docker Compose deployment
- [x] Incremental sync with checkpointing schema
- [ ] EVM native ETH transfer and gas fee parsing
- [ ] Improved entry type classification (trades, staking)
- [ ] Historical price lookups for fiat value
- [ ] API authentication and authorization
- [ ] User identity and wallet management
- [ ] Cross-chain portfolio views

## License

See [LICENSE](LICENSE) for details.
