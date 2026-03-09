# Spectraplex

[![CI](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/ci.yml)
[![Security Audit](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml/badge.svg)](https://github.com/user1303836/spectraplex/actions/workflows/audit.yml)

Spectraplex is a Rust multi-chain indexing and normalization system for blockchain ETL. It ingests raw chain data into canonical data tiers (Bronze records, materializes reusable Silver and Gold datasets) and exposes both compatibility wallet endpoints and newer target- and dataset-oriented APIs.

The project treats wallets, contracts, programs, markets, pools, and protocol analytics as first-class indexing targets.

## What Exists Today

- Bronze storage for raw chain transactions and events
- Silver datasets for transfers, balance deltas, decoded events, fills, funding, and positions
- Gold datasets for wallet ledger, balance history, Hyperliquid analytics, protocol activity, and TVL
- Legacy wallet-centric CLI and API flows
- V2 target and network primitives for broader indexing use cases
- Export jobs with JSONL and CSV output

## Supported Chains and Networks

Current ingestion adapters:

| Family | Chain / Source | Current status |
|--------|-----------------|----------------|
| Solana | RPC and Yellowstone gRPC | Implemented |
| EVM | `eth_getLogs` via `alloy` | Implemented |
| Hyperliquid | REST and WebSocket | Implemented |

Seeded V2 networks after `init-db`:

| Network ID | Family | Notes |
|-----------|--------|-------|
| `solana-mainnet` | `solana` | probabilistic-slot finality |
| `ethereum-mainnet` | `evm` | probabilistic-block finality |
| `base-mainnet` | `evm` | deterministic-block finality |
| `arbitrum-mainnet` | `evm` | deterministic-block finality |
| `hyperevm-mainnet` | `evm` | deterministic-block finality |
| `hypercore-mainnet` | `hyperliquid` | instant finality |

## Repository Layout

| Path | Purpose |
|------|---------|
| `core/` | shared config, V1/V2 models, target and materializer types |
| `adapters/` | chain adapters, parsers, dataset derivation, repository layer |
| `cli/` | CLI entry points for DB setup, ingestion, normalization, and V2 target management |
| `api/` | Axum API server for ingestion, queries, exports, and analytics |
| `migrations/` | PostgreSQL schema and seed data |

## Quick Start

### Prerequisites

- Rust stable
- PostgreSQL 14+
- Docker, if you want the bundled local database

### 1. Clone and build

```bash
git clone https://github.com/user1303836/spectraplex.git
cd spectraplex
cargo build --workspace
```

### 2. Start PostgreSQL

```bash
docker-compose up -d
```

Or point `DATABASE_URL` at an existing PostgreSQL instance.

### 3. Configure the app

Spectraplex loads configuration from:

1. defaults
2. `spectraplex.toml`
3. `SPECTRAPLEX_*` environment variables
4. direct env vars such as `DATABASE_URL`

Start from the example file if you want a local config:

```bash
cp spectraplex.toml.example spectraplex.toml
```

Most useful environment variables:

```bash
export DATABASE_URL=postgresql://localhost/spectraplex
export SPECTRAPLEX_API_KEY=change-me
export SOLANA_RPC_URL=https://api.mainnet-beta.solana.com
export EVM_RPC_URL=https://eth.llamarpc.com
# Optional:
export SOLANA_GRPC_URL=https://your-yellowstone-endpoint
export SOLANA_GRPC_TOKEN=your-token
export SPECTRAPLEX_ALLOWED_WALLETS=<wallet1>,<wallet2>
```

### 4. Initialize the schema

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" init-db
```

### 5. Inspect seeded networks

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" list-networks
```

### 6. Ingest data

Wallet compatibility path:

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" ingest \
  --chain solana \
  --wallet <WALLET_ADDRESS> \
  --rpc "$SOLANA_RPC_URL" \
  --limit 10
```

Solana gRPC:

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" ingest \
  --chain solana \
  --wallet <WALLET_ADDRESS> \
  --grpc-url "$SOLANA_GRPC_URL" \
  --x-token "$SOLANA_GRPC_TOKEN" \
  --limit 10
```

EVM:

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" ingest \
  --chain ethereum \
  --wallet <EVM_ADDRESS> \
  --rpc "$EVM_RPC_URL" \
  --limit 10
```

Hyperliquid:

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" ingest \
  --chain hyperliquid \
  --wallet <HYPERLIQUID_ADDRESS> \
  --limit 10
```

### 7. Normalize legacy wallet data

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" normalize \
  --input db:<WALLET_ADDRESS>
```

### 8. Register a V2 target

Wallet target:

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" register-target \
  --kind wallet \
  --network solana-mainnet \
  --address <WALLET_ADDRESS> \
  --mode both \
  --label "Primary wallet"
```

Topic filter target:

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" register-target \
  --kind topic_filter \
  --network ethereum-mainnet \
  --filter-spec '{"topics":["0xddf252ad00000000000000000000000000000000000000000000000000000000"]}' \
  --mode backfill \
  --label "ERC20 transfers"
```

### 9. Start the API

```bash
cargo run --bin spectraplex-api
```

The API binds to `SPECTRAPLEX_HOST:SPECTRAPLEX_PORT`, defaulting to `127.0.0.1:3000`.

## CLI Commands

```bash
cargo run --bin spectraplex-cli -- --help
```

Current commands:

- `init-db`
- `ingest`
- `normalize`
- `register-target`
- `list-targets`
- `list-networks`

Examples:

```bash
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" list-targets
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" list-targets --network solana-mainnet
cargo run --bin spectraplex-cli -- --db-url "$DATABASE_URL" list-targets --kind wallet
```

## API Overview

All `/v1/*` routes require:

```text
Authorization: Bearer <SPECTRAPLEX_API_KEY>
```

`/health` does not require auth.

### Wallet Compatibility Endpoints

- `POST /v1/ingest`
- `POST /v1/ingest/batch`
- `POST /v1/normalize`
- `GET /v1/jobs/:job_id`
- `GET /v1/transactions/:wallet`
- `GET /v1/transactions/:wallet/:tx_hash`
- `GET /v1/ledger/:wallet`
- `GET /v1/export/:wallet`
- `GET /v1/balances/:wallet`
- `GET /v1/stats/:wallet`
- `POST /v1/stream/start`
- `POST /v1/stream/:stream_id/stop`
- `GET /v1/streams`

### Target, Network, and Dataset Endpoints

- `POST /v1/targets`
- `GET /v1/targets`
- `GET /v1/targets/:target_id`
- `GET /v1/networks`
- `GET /v1/networks/:network_id`
- `GET /v1/datasets`
- `GET /v1/datasets/:name/versions`
- `GET /v1/datasets/:name/records`
- `GET /v1/datasets/:name/completeness`
- `GET /v1/datasets/:name/status`
- `POST /v1/export/dataset`
- `GET /v1/export/jobs/:job_id`
- `GET /v1/export/jobs/:job_id/download`

### Analytics and Downstream-Oriented Endpoints

- `GET /v1/export/tax`
- `GET /v1/forensics/activity`
- `GET /v1/analytics/hl/trader`
- `GET /v1/analytics/hl/market`
- `GET /v1/analytics/protocol/activity`
- `GET /v1/analytics/protocol/tvl`

### Example API Calls

Health:

```bash
curl http://127.0.0.1:3000/health
```

List networks:

```bash
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  http://127.0.0.1:3000/v1/networks
```

Register a target:

```bash
curl -X POST http://127.0.0.1:3000/v1/targets \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "kind": "wallet",
    "network": "solana-mainnet",
    "address": "<WALLET_ADDRESS>",
    "mode": "both",
    "label": "Primary wallet"
  }'
```

Query a dataset:

```bash
curl -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  "http://127.0.0.1:3000/v1/datasets/token_transfers/records?network=solana-mainnet&limit=50"
```

Create a dataset export job:

```bash
curl -X POST http://127.0.0.1:3000/v1/export/dataset \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "dataset": "wallet_ledger",
    "format": "csv",
    "network": "solana-mainnet"
  }'
```

Dataset export jobs support optional sinks:

- `local_file`
- `webhook`

Example webhook sink:

```bash
curl -X POST http://127.0.0.1:3000/v1/export/dataset \
  -H "Authorization: Bearer $SPECTRAPLEX_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "dataset": "protocol_events",
    "format": "jsonl",
    "sink": {
      "sink_type": "webhook",
      "url": "https://example.com/ingest"
    }
  }'
```

## Queryable and Exportable Datasets

Datasets exposed today:

- `token_transfers`
- `native_balance_deltas`
- `decoded_events`
- `hl_fills`
- `hl_funding`
- `positions`
- `wallet_ledger`
- `balance_history`
- `hl_pnl_summary`
- `hl_trade_history`
- `protocol_events`
- `pool_snapshots`

## Verification Commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Current Caveats

- Wallet ingestion and wallet query paths are still the most mature parts of the system.
- V2 target registration and dataset APIs exist, but ingestion orchestration is still uneven across target kinds.
- Local planning and research files are intentionally kept out of version control.
