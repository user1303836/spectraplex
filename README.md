# Spectraplex

[![CI](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml)
[![Security Audit](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml)

Multi-chain blockchain indexer and ETL pipeline. Ingest raw data from Solana, EVM chains, and Hyperliquid, normalize it into queryable datasets, and export it as CSV or JSONL.

## Quick Start

```bash
# Clone and build
git clone https://github.com/user1303836/spectraplex.git
cd spectraplex
cargo build --workspace

# Start PostgreSQL
docker-compose up -d

# Start the API server (auto-runs migrations on first start)
cargo run --bin spectraplex-api
# → http://127.0.0.1:3000/health

# Ingest some Solana transactions
cargo run --bin spectraplex-cli -- --db-url postgresql://localhost/spectraplex ingest \
  --chain solana --wallet <WALLET_ADDRESS> --rpc https://api.mainnet-beta.solana.com --limit 10
```

Requires Rust (stable) and PostgreSQL 15+. Docker handles Postgres if you don't have one running.

## Features

**Ingest from any supported chain with a single command.** Point at a wallet, contract, or event filter and Spectraplex pulls raw transactions, logs, and fills into Bronze storage.

**Bronze-native Silver materialization.** Raw data is extracted directly into canonical Silver datasets — token transfers, balance deltas, decoded events, fills, funding, and positions — without V1 compatibility overhead.

**Durable control plane.** Ingestion jobs, stream subscriptions, export jobs, and materialization runs are all Postgres-backed with worker loops, heartbeats, lease-based ownership, and restart recovery.

**Real-time streaming with full lineage.** Solana gRPC and Hyperliquid WebSocket streams create ingestion runs, persist V2 checkpoints, and auto-trigger downstream materialization — the same lineage model as backfill.

**Query and export structured datasets.** Silver and Gold datasets are available through a REST API with filtering, pagination, and async export to CSV, JSONL, local files, or webhooks.

**V2-backed wallet reads.** Legacy wallet endpoints (`/v1/transactions/:wallet`, `/v1/ledger/:wallet`, etc.) are served from V2 tables behind unchanged response shapes.

**Built-in analytics endpoints.** Trader PnL, market stats, protocol activity, and TVL queries ship out of the box.

## Architecture

Data flows through three tiers:

```
Ingestion → Bronze (raw_transactions) → Silver (token_transfers, etc.) → Gold (wallet_ledger, etc.)
                ↓                              ↓
         target_matches              dataset_completeness
         ingestion_runs              dataset_versions
         V2 checkpoints              dataset_watermarks
```

- **Bronze** is canonical and target-agnostic. Raw chain data lives in `raw_transactions` and is linked to `ingestion_runs` and `target_matches`.
- **Silver** datasets are extracted directly from Bronze using chain-specific parsers. Every Silver row carries `raw_transaction_id` for provenance.
- **Gold** datasets are derived from Silver for wallet, tax, forensics, and analytics use cases.

All operational state — jobs, streams, exports, materialization runs — is durable in PostgreSQL. No runtime truth lives in in-memory maps.

## Supported Chains

| Chain | Data Source | Networks |
|-------|------------|----------|
| **Solana** | RPC + Yellowstone gRPC | `solana-mainnet` |
| **Ethereum** | `eth_getLogs` via alloy | `ethereum-mainnet` |
| **Base** | `eth_getLogs` via alloy | `base-mainnet` |
| **Arbitrum** | `eth_getLogs` via alloy | `arbitrum-mainnet` |
| **HyperEVM** | `eth_getLogs` via alloy | `hyperevm-mainnet` |
| **Hyperliquid** | REST + WebSocket | `hypercore-mainnet` |

## Datasets

| Dataset | Tier | Description |
|---------|------|-------------|
| `token_transfers` | Silver | Token transfer records across all chains |
| `native_balance_deltas` | Silver | Native currency balance changes per account |
| `decoded_events` | Silver | ABI-decoded EVM events and Solana logs |
| `hl_fills` | Silver | Hyperliquid trade fills |
| `hl_funding` | Silver | Hyperliquid funding payments |
| `positions` | Silver | Position state changes from fills and liquidations |
| `wallet_ledger` | Gold | Ledger entries with counterparty tracking |
| `balance_history` | Gold | Per-asset balance snapshots over time |
| `hl_pnl_summary` | Gold | Hyperliquid PnL per coin per period |
| `hl_trade_history` | Gold | Hyperliquid trades with entry/exit grouping |
| `protocol_events` | Gold | Protocol events derived from decoded logs |
| `pool_snapshots` | Gold | Pool state snapshots from events and transfers |

## API

All `/v1/*` routes require `Authorization: Bearer <SPECTRAPLEX_API_KEY>`.

### Ingestion and Jobs

```bash
# Trigger ingestion
curl -X POST http://127.0.0.1:3000/v1/ingest \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"chain": "solana", "wallet": "<ADDRESS>", "rpc_url": "https://api.mainnet-beta.solana.com"}'

# Check job status
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  http://127.0.0.1:3000/v1/jobs/<JOB_ID>
```

Ingestion jobs are durable — they persist across restarts and are claimed by background workers with heartbeat-based lease management.

### Real-Time Streaming

```bash
# Start a Solana gRPC stream (requires SOLANA_GRPC_URL)
curl -X POST http://127.0.0.1:3000/v1/stream/start \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"chain": "solana"}'

# Start a Hyperliquid WebSocket stream for a wallet
curl -X POST http://127.0.0.1:3000/v1/stream/start \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"chain": "hyperliquid", "wallet": "0x..."}'

# List active streams
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  http://127.0.0.1:3000/v1/streams

# Stop a stream
curl -X POST http://127.0.0.1:3000/v1/stream/<STREAM_ID>/stop \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY"
```

Stream subscriptions are durable and create `ingestion_runs` per flush batch with full Bronze lineage. Hyperliquid streams subscribe to user fills, funding, and ledger updates via WebSocket with automatic reconnection.

### Targets and Networks

```bash
# Register an indexing target
curl -X POST http://127.0.0.1:3000/v1/targets \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"kind": "wallet", "network": "solana-mainnet", "address": "<ADDRESS>", "mode": "both"}'

# List networks
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" http://127.0.0.1:3000/v1/networks
```

### Query Datasets

```bash
# Query any dataset with filters
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  "http://127.0.0.1:3000/v1/datasets/token_transfers/records?network=solana-mainnet&limit=50"

# Check dataset completeness
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  http://127.0.0.1:3000/v1/datasets/token_transfers/completeness
```

### Export

```bash
# Create an export job (CSV or JSONL)
curl -X POST http://127.0.0.1:3000/v1/export/dataset \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"dataset": "wallet_ledger", "format": "csv", "network": "solana-mainnet"}'

# Download when ready
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  http://127.0.0.1:3000/v1/export/jobs/<JOB_ID>/download
```

Export jobs are durable with heartbeat-based worker execution. Supported sinks: `local_file` and `webhook`.

### Analytics

```bash
# Hyperliquid trader analytics
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  "http://127.0.0.1:3000/v1/analytics/hl/trader?wallet=<ADDRESS>"

# Protocol activity
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  "http://127.0.0.1:3000/v1/analytics/protocol/activity?network=ethereum-mainnet"

# TVL
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  http://127.0.0.1:3000/v1/analytics/protocol/tvl
```

<details>
<summary>Full endpoint reference</summary>

**Ingestion and job control:**
`POST /v1/ingest` | `POST /v1/ingest/batch` | `POST /v1/normalize` | `GET /v1/jobs/:job_id` | `POST /v1/stream/start` | `POST /v1/stream/:stream_id/stop` | `GET /v1/streams`

**Wallet endpoints (V2-backed):**
`GET /v1/transactions/:wallet` | `GET /v1/transactions/:wallet/:tx_hash` | `GET /v1/ledger/:wallet` | `GET /v1/export/:wallet` | `GET /v1/balances/:wallet` | `GET /v1/stats/:wallet`

**Targets and networks:**
`POST /v1/targets` | `GET /v1/targets` | `GET /v1/targets/:target_id` | `GET /v1/networks` | `GET /v1/networks/:network_id`

**Datasets:**
`GET /v1/datasets` | `GET /v1/datasets/:name/versions` | `GET /v1/datasets/:name/records` | `GET /v1/datasets/:name/completeness` | `GET /v1/datasets/:name/status`

**Export:**
`POST /v1/export/dataset` | `GET /v1/export/jobs/:job_id` | `GET /v1/export/jobs/:job_id/download` | `GET /v1/export/tax`

**Analytics:**
`GET /v1/forensics/activity` | `GET /v1/analytics/hl/trader` | `GET /v1/analytics/hl/market` | `GET /v1/analytics/protocol/activity` | `GET /v1/analytics/protocol/tvl`

</details>

## CLI

```bash
cargo run --bin spectraplex-cli -- --help
```

| Command | Description |
|---------|-------------|
| `init-db` | Create tables, indexes, and seed network data |
| `ingest` | Pull raw transactions from a chain into Bronze storage |
| `normalize` | Materialize Silver datasets from ingested Bronze data |
| `register-target` | Register a wallet, contract, program, or event filter for indexing |
| `list-targets` | List registered targets with optional network/kind filters |
| `list-networks` | Show all seeded networks |

```bash
# Ingest from any chain
spectraplex-cli --db-url "$DATABASE_URL" ingest --chain ethereum --wallet <ADDRESS> --rpc "$EVM_RPC_URL" --limit 10
spectraplex-cli --db-url "$DATABASE_URL" ingest --chain hyperliquid --wallet <ADDRESS> --limit 10

# Register a target for ERC-20 Transfer events
spectraplex-cli --db-url "$DATABASE_URL" register-target \
  --kind topic_filter --network ethereum-mainnet \
  --filter-spec '{"topics":["0xddf252ad00000000000000000000000000000000000000000000000000000000"]}' \
  --mode backfill --label "ERC20 transfers"
```

## Configuration

Spectraplex loads config from (in order of precedence):

1. Defaults
2. `spectraplex.toml` (copy from `spectraplex.toml.example`)
3. `SPECTRAPLEX_*` environment variables
4. Direct env vars (`DATABASE_URL`, `SOLANA_RPC_URL`, etc.)

Key environment variables:

```bash
DATABASE_URL=postgresql://localhost/spectraplex
SPECTRAPLEX_API_KEY=your-api-key
SOLANA_RPC_URL=https://api.mainnet-beta.solana.com
EVM_RPC_URL=https://eth.llamarpc.com
# Optional
SOLANA_GRPC_URL=https://your-yellowstone-endpoint
SOLANA_GRPC_TOKEN=your-grpc-token
```

## Project Layout

```
spectraplex/
├── core/         Shared models, config, dataset registry, traits
├── adapters/     Chain adapters, parsers, dual-write layer, repository
├── cli/          CLI entry points
├── api/          Axum HTTP API server + background workers
└── migrations/   PostgreSQL schema and seed data
```

## Development

```bash
cargo fmt --all --check                                    # format check
cargo clippy --workspace --all-targets -- -D warnings      # lint
cargo test --workspace                                     # tests
```

## License

MIT
